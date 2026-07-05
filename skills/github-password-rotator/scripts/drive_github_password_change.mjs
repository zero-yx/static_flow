#!/usr/bin/env node

import { randomInt } from "node:crypto";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const port = process.env.GITHUB_DEVTOOLS_PORT;
const githubLogin = process.env.GITHUB_LOGIN || "";
const currentPassword = process.env.GITHUB_CURRENT_PASSWORD || "";
const newPassword = process.env.GITHUB_NEW_PASSWORD || "";
const settingsUrl = process.env.GITHUB_SETTINGS_URL || "https://github.com/settings/security";
const timeoutSeconds = Number(process.env.GITHUB_MANUAL_TIMEOUT_SECONDS || "900");
const createLearningRepo = process.env.GITHUB_CREATE_LEARNING_REPO === "1";
const learningRepoOwner = process.env.GITHUB_LEARNING_REPO_OWNER || githubLogin;
const auto2faFun = process.env.GITHUB_AUTO_2FA_FUN === "1";
const totpSecret = process.env.GITHUB_TOTP_SECRET || "";
const loginOnly = process.env.GITHUB_LOGIN_ONLY === "1";
const isMain = process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1]);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function connectPage() {
  const deadline = Date.now() + 25_000;
  while (Date.now() < deadline) {
    try {
      const pages = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = pages.find((item) => item.type === "page");
      if (page?.webSocketDebuggerUrl) {
        return page;
      }
    } catch {
      // Chrome may still be starting.
    }
    await sleep(250);
  }
  throw new Error("Chrome DevTools page target not found");
}

let ws;
let nextId = 0;
const pending = new Map();

async function openWebsocket() {
  const page = await connectPage();
  ws = new WebSocket(page.webSocketDebuggerUrl);
  ws.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  };

  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = reject;
  });
}

function send(method, params = {}) {
  return new Promise((resolve) => {
    if (!ws) {
      throw new Error("Chrome DevTools websocket is not connected");
    }
    const id = ++nextId;
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params }));
  });
}

async function evalJs(expression) {
  const response = await send("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (response.exceptionDetails) {
    throw new Error(JSON.stringify(response.exceptionDetails));
  }
  return response.result?.result?.value;
}

function jsString(value) {
  return JSON.stringify(value);
}

async function navigate(url) {
  await send("Page.navigate", { url });
}

async function waitForPageSettle(ms = 1500) {
  await sleep(ms);
}

async function state() {
  return await evalJs(`(() => ({
    title: document.title,
    url: location.href,
    text: document.body ? document.body.innerText.slice(0, 3000) : "",
    hasLoginInput: !!document.querySelector('#login_field,input[name="login"],input[name="user_login"],input[type="email"]'),
    passwordInputs: [...document.querySelectorAll('input[type="password"]')]
      .map((e) => ({
        id: e.id || "",
        name: e.name || "",
        autocomplete: e.autocomplete || "",
        placeholder: e.placeholder || "",
        visible: !!(e.offsetWidth || e.offsetHeight || e.getClientRects().length),
      })),
    buttons: [...document.querySelectorAll('button,a,[role="button"],input[type="submit"]')]
      .map((e) => (e.innerText || e.value || e.getAttribute('aria-label') || '').trim())
      .filter(Boolean)
      .slice(0, 100),
  }))()`);
}

async function clickText(label) {
  return await evalJs(`(() => {
    const target = ${jsString(label)};
    const primary = [...document.querySelectorAll('button,a,[role="button"],input[type="submit"]')];
    const el = primary.find((e) => (e.innerText || e.value || e.getAttribute('aria-label') || '').trim() === target);
    if (!el) return false;
    el.click();
    return true;
  })()`);
}

async function clickTextContaining(fragment) {
  return await evalJs(`(() => {
    const target = ${jsString(fragment)}.toLowerCase();
    const primary = [...document.querySelectorAll('button,a,[role="button"],input[type="submit"]')];
    const el = primary.find((e) => ((e.innerText || e.value || e.getAttribute('aria-label') || '').trim().toLowerCase()).includes(target));
    if (!el) return false;
    el.click();
    return true;
  })()`);
}

async function clickEnabledTextContaining(fragment) {
  return await evalJs(`(() => {
    const target = ${jsString(fragment)}.toLowerCase();
    const primary = [...document.querySelectorAll('button,a,[role="button"],input[type="submit"]')];
    const el = primary.find((e) => {
      const text = ((e.innerText || e.value || e.getAttribute('aria-label') || '').trim().toLowerCase());
      return text.includes(target) && !e.disabled && e.getAttribute('aria-disabled') !== 'true';
    });
    if (!el) return false;
    el.scrollIntoView({ block: 'center' });
    el.click();
    return true;
  })()`);
}

async function browserPageTargets() {
  return await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
}

async function targetForUrl(fragment) {
  const targets = await browserPageTargets();
  return targets.find((item) => item.type === "page" && item.url.includes(fragment));
}

async function openOrFindPage(fragment, url) {
  const existing = await targetForUrl(fragment);
  if (existing?.webSocketDebuggerUrl) {
    return existing;
  }
  await fetch(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(url)}`, { method: "PUT" });
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const target = await targetForUrl(fragment);
    if (target?.webSocketDebuggerUrl) {
      return target;
    }
    await sleep(500);
  }
  throw new Error(`Browser page did not open: ${url}`);
}

async function evaluateInTarget(target, expression) {
  const targetWs = new WebSocket(target.webSocketDebuggerUrl);
  let targetId = 0;
  const targetPending = new Map();
  targetWs.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (targetPending.has(message.id)) {
      targetPending.get(message.id)(message);
      targetPending.delete(message.id);
    }
  };
  await new Promise((resolve, reject) => {
    targetWs.onopen = resolve;
    targetWs.onerror = reject;
  });
  const targetSend = (method, params = {}) =>
    new Promise((resolve) => {
      const id = ++targetId;
      targetPending.set(id, resolve);
      targetWs.send(JSON.stringify({ id, method, params }));
    });
  await targetSend("Runtime.enable");
  const response = await targetSend("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  targetWs.close();
  if (response.exceptionDetails) {
    throw new Error(JSON.stringify(response.exceptionDetails));
  }
  return response.result?.result?.value;
}

async function setInput(selector, value) {
  return await evalJs(`(() => {
    const e = document.querySelector(${jsString(selector)});
    if (!e) return false;
    e.scrollIntoView({ block: 'center' });
    e.focus();
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(e, ${jsString(value)});
    e.dispatchEvent(new Event('input', { bubbles: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  })()`);
}

async function setFileEditorContent(content) {
  const mode = await evalJs(`(() => {
    const value = ${jsString(content)};
    const setTextControl = (e) => {
      e.scrollIntoView({ block: 'center' });
      e.focus();
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set;
      if (setter) {
        setter.call(e, value);
      } else {
        e.value = value;
      }
      e.dispatchEvent(new Event('input', { bubbles: true }));
      e.dispatchEvent(new Event('change', { bubbles: true }));
      return 'text-control';
    };
    const textarea = document.querySelector('textarea[name="value"],textarea[aria-label*="file"],textarea[aria-label*="File"],textarea');
    if (textarea) return setTextControl(textarea);
    if (window.monaco?.editor?.getModels?.()?.length) {
      window.monaco.editor.getModels()[0].setValue(value);
      return 'monaco';
    }
    const cm = document.querySelector('.cm-content[contenteditable="true"],[role="textbox"][contenteditable="true"]');
    if (cm) {
      cm.scrollIntoView({ block: 'center' });
      const rect = cm.getBoundingClientRect();
      return {
        mode: 'contenteditable',
        x: rect.x + 20,
        y: rect.y + 20,
      };
    }
    return { mode: '' };
  })()`);
  if (mode?.mode === "contenteditable") {
    await send("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x: mode.x,
      y: mode.y,
      button: "none",
    });
    await send("Input.dispatchMouseEvent", {
      type: "mousePressed",
      x: mode.x,
      y: mode.y,
      button: "left",
      clickCount: 1,
    });
    await send("Input.dispatchMouseEvent", {
      type: "mouseReleased",
      x: mode.x,
      y: mode.y,
      button: "left",
      clickCount: 1,
    });
    await sleep(300);
    await send("Input.insertText", { text: content });
    await sleep(500);
    return true;
  }
  if (typeof mode === "string") {
    return !!mode;
  }
  return !!mode?.mode;
}

async function setPasswordByIndex(index, value) {
  return await evalJs(`(() => {
    const visible = [...document.querySelectorAll('input[type="password"]')]
      .filter((e) => !!(e.offsetWidth || e.offsetHeight || e.getClientRects().length));
    const e = visible[${index}];
    if (!e) return false;
    e.focus();
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(e, ${jsString(value)});
    e.dispatchEvent(new Event('input', { bubbles: true }));
    e.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  })()`);
}

async function submitNearestPasswordForm() {
  return await evalJs(`(() => {
    const password = document.querySelector('input[type="password"]');
    const form = password ? password.closest('form') : null;
    if (form) {
      const submit = form.querySelector('button[type="submit"],input[type="submit"],button');
      if (submit) {
        submit.click();
        return true;
      }
      form.requestSubmit();
      return true;
    }
    return false;
  })()`);
}

function isGithubLoginPage(url, lower) {
  return (
    url.includes("github.com") &&
    (url.includes("/login") ||
      lower.includes("sign in to github") ||
      lower.includes("username or email address"))
  );
}

function isGithubSettingsPage(url) {
  try {
    const parsed = new URL(url);
    return parsed.hostname === "github.com" && parsed.pathname.startsWith("/settings/");
  } catch {
    return url.includes("github.com/settings/");
  }
}

function requiresManualGithubStep(url, lower) {
  const accountRestriction =
    lower.includes("account suspended") ||
    lower.includes("account has been suspended") ||
    lower.includes("account disabled") ||
    lower.includes("account has been disabled") ||
    lower.includes("your account has been disabled");
  if (accountRestriction) {
    return true;
  }
  if (isGithubSettingsPage(url)) {
    return false;
  }
  return (
    url.includes("github.com/sessions/two-factor") ||
    url.includes("github.com/sessions/verified-device") ||
    url.includes("github.com/sessions/webauthn") ||
    url.includes("github.com/login/device") ||
    lower.includes("authentication code") ||
    lower.includes("verify your identity") ||
    lower.includes("verify your account") ||
    lower.includes("device verification") ||
    lower.includes("enter the code") ||
    (lower.includes("code") && lower.includes("we sent")) ||
    ((lower.includes("passkey") || lower.includes("security key")) &&
      (lower.includes("authenticate") || lower.includes("sign in") || lower.includes("verify")))
  );
}

function isGithubTwoFactorPrompt(url, lower) {
  return (
    url.includes("github.com/sessions/two-factor") ||
    url.includes("github.com/settings/two_factor_checkup") ||
    lower.includes("authentication code") ||
    lower.includes("two-factor authentication") ||
    lower.includes("verify your two-factor authentication") ||
    lower.includes("verify your recently configured two-factor authentication method")
  );
}

function extractTotpCodeFrom2faFunValues(values) {
  for (const value of values) {
    const match = String(value || "").match(/^\s*(\d{6})\s*$/) || String(value || "").match(/\b(\d{6})\b/);
    if (match) {
      return match[1];
    }
  }
  return "";
}

async function codeFrom2faFun() {
  if (!totpSecret) {
    return "";
  }
  const target = await openOrFindPage("2fa.fun", "https://2fa.fun/");
  const inputOk = await evaluateInTarget(
    target,
    `(() => {
      const textarea = document.querySelector('#SECRET2FA,textarea[name="SECRET2FA"],textarea');
      if (!textarea) return false;
      textarea.focus();
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
      setter.call(textarea, ${jsString(totpSecret)});
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
      textarea.dispatchEvent(new Event('change', { bubbles: true }));
      const button = [...document.querySelectorAll('button,input[type=submit],[role=button]')]
        .find((e) => /获取验证码|验证码|get|code/i.test((e.innerText || e.value || e.getAttribute('aria-label') || '').trim()));
      if (!button) return false;
      button.click();
      return true;
    })()`
  );
  if (!inputOk) {
    return "";
  }

  const deadline = Date.now() + 12_000;
  while (Date.now() < deadline) {
    await sleep(500);
    const values = await evaluateInTarget(
      target,
      `(() => [...document.querySelectorAll('input.faotp')].map((e) => e.value || ''))()`
    );
    const code = extractTotpCodeFrom2faFunValues(values || []);
    if (code) {
      return code;
    }
  }
  return "";
}

async function submitGithubTotpCode(code) {
  if (!code) {
    return false;
  }
  return await evalJs(`(() => {
    const code = ${jsString(code)};
    const input = document.querySelector('input[name="app_otp"],input#app_totp,input[name="otp"],input[name="two_factor_otp"],input[autocomplete="one-time-code"],input[type="text"],input[type="tel"]');
    if (!input) return false;
    input.focus();
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(input, code);
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new Event('change', { bubbles: true }));
    const form = input.closest('form');
    const button = form?.querySelector('button[type="submit"],input[type="submit"],button') ||
      [...document.querySelectorAll('button,input[type=submit]')]
        .find((e) => /verify|submit|continue/i.test((e.innerText || e.value || '').trim()));
    if (button) {
      button.click();
      return true;
    }
    if (form) {
      form.requestSubmit();
      return true;
    }
    return false;
  })()`);
}

function hasPasswordChangeForm(current) {
  const inputs = current.passwordInputs || [];
  const names = inputs.map((item) => `${item.id} ${item.name} ${item.autocomplete}`.toLowerCase());
  return (
    inputs.length >= 3 ||
    names.some((text) => text.includes("old_password")) ||
    names.some((text) => text.includes("password_confirmation"))
  );
}

function looksLikeSuccess(lower) {
  return (
    lower.includes("password was successfully updated") ||
    lower.includes("password has been updated") ||
    lower.includes("your password was changed") ||
    lower.includes("password changed successfully")
  );
}

function accountSlug(accountName) {
  const slug = (accountName || "github-user")
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 80);
  return slug || "github-user";
}

function learningRepoNameForAccount(accountName) {
  return `hello-world-from-${accountSlug(accountName)}`;
}

function randomPostPasswordChangeDelayMs(pickInt = randomInt) {
  return 3000 + pickInt(7001);
}

function pickVariant(variants, pickIndex = randomInt) {
  return variants[pickIndex(variants.length)];
}

function pickDistinct(variants, count, pickIndex = randomInt) {
  const remaining = [...variants];
  const picked = [];
  while (remaining.length > 0 && picked.length < count) {
    const index = pickIndex(remaining.length);
    picked.push(remaining.splice(index, 1)[0]);
  }
  return picked;
}

function learningRepoDescriptionForAccount(accountName, pickIndex = randomInt) {
  const displayName = accountName || "this GitHub account";
  const variants = [
    `A small beginner-friendly practice repository for ${displayName}.`,
    `A first GitHub learning space for ${displayName}.`,
    `A simple starter project for practicing GitHub basics as ${displayName}.`,
    `A lightweight hello-world repository for ${displayName}'s GitHub practice.`,
  ];
  return pickVariant(variants, pickIndex);
}

function learningRepoContentForAccount(accountName, pickIndex = randomInt) {
  const displayName = accountName || "this account";
  const intro = pickVariant([
    `This repository is a small learning space for ${displayName}. It keeps the first GitHub project simple: a README, a few notes, and room to practice commits, branches, and pull requests.`,
    `${displayName} can use this repository as a gentle starting point for GitHub. The goal is to learn how a project is organized, how changes are saved, and how simple documentation helps other people understand the work.`,
    `This is a beginner practice repository for ${displayName}. It is intentionally small so the focus stays on learning the basics: editing files, writing clear commit messages, and getting comfortable with the GitHub workflow.`,
    `Welcome to ${displayName}'s first practice repository. This project is for learning by doing, starting with a simple README and growing through small, easy-to-review updates.`,
  ], pickIndex);
  const learningGoals = pickDistinct([
    "Practice editing files in a repository.",
    "Learn how commits record project history.",
    "Use branches and pull requests for small changes.",
    "Keep project notes clear enough for another beginner to follow.",
    "Try Markdown headings, lists, and links.",
    "Review changes before merging them.",
  ], 4, pickIndex);
  const exercise = pickVariant([
    "Add one short note about something learned today, commit it, and review the change on GitHub.",
    "Create a new branch, edit this README, and open a pull request with a short explanation.",
    "Add a tiny checklist for the next practice session, then commit it with a clear message.",
    "Write a short paragraph about what a repository is and save it as the next commit.",
  ], pickIndex);
  return [
    `# Hello World from ${displayName}`,
    "",
    intro,
    "",
    "## Learning goals",
    "",
    ...learningGoals.map((goal) => `- ${goal}`),
    "",
    "## First exercise",
    "",
    exercise,
    "",
  ].join("\n");
}

async function waitForState(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  let current;
  while (Date.now() < deadline) {
    current = await state();
    if (predicate(current)) {
      return current;
    }
    await sleep(1000);
  }
  throw new Error(`Timed out waiting for ${label}; last url=${current?.url}; title=${current?.title}`);
}

async function createLearningRepository() {
  const repoName = learningRepoNameForAccount(learningRepoOwner);
  const description = learningRepoDescriptionForAccount(learningRepoOwner);
  const readme = learningRepoContentForAccount(learningRepoOwner);
  const ownerSlug = accountSlug(learningRepoOwner);

  await navigate("https://github.com/new");
  await waitForPageSettle(3000);
  let current = await state();
  const currentLower = (current.text || "").toLowerCase();
  if (isGithubTwoFactorPrompt(current.url, currentLower)) {
    if (auto2faFun && totpSecret) {
      await clickEnabledTextContaining("verify 2fa now");
      await sleep(3000);
      const code = await codeFrom2faFun();
      if (await submitGithubTotpCode(code)) {
        console.log("Browser helper: submitted GitHub 2FA code from 2fa.fun");
        await sleep(3500);
      }
      await navigate("https://github.com/new");
      await waitForPageSettle(3000);
    } else if (await clickEnabledTextContaining("skip 2fa verification")) {
      await waitForPageSettle(3000);
    }
  }

  const nameSet = await setInput('#repository-name-input,input[aria-label="Repository name"],input[id*="repository-name"]', repoName);
  const descriptionSet = await setInput('input[name="Description"],input[aria-label="Description"],input[id*="description"]', description);
  if (!nameSet) {
    throw new Error("GitHub repository name field was not found");
  }
  if (!descriptionSet) {
    throw new Error("GitHub repository description field was not found");
  }

  await sleep(2500);
  const createClicked = await clickEnabledTextContaining("create repository");
  if (!createClicked) {
    throw new Error("GitHub create repository button was not available");
  }

  await waitForState(
    (current) =>
      current.url.toLowerCase().includes(`/${ownerSlug}/${repoName}`) ||
      current.text.toLowerCase().includes("quick setup"),
    45_000,
    `repository ${repoName} to be created`
  );

  await navigate(`https://github.com/${learningRepoOwner}/${repoName}/new/main?filename=README.md`);
  await waitForPageSettle(3500);
  await setInput('input[name="filename"],input[aria-label*="file name"],input[placeholder*="Name your file"]', "README.md");
  const contentSet = await setFileEditorContent(readme);
  if (!contentSet) {
    throw new Error("GitHub file editor was not found for README.md");
  }
  await sleep(1000);
  const firstCommitClicked = await clickEnabledTextContaining("commit changes");
  if (!firstCommitClicked) {
    throw new Error("GitHub commit button was not available for README.md");
  }
  await sleep(1500);
  await clickEnabledTextContaining("commit changes");

  const finalState = await waitForState(
    (current) => learningRepoReadmeCommitted(current, ownerSlug, repoName),
    45_000,
    `README.md commit in ${repoName}`
  );
  return { repoName, url: finalState.url };
}

function learningRepoReadmeCommitted(current, ownerSlug, repoName) {
  const url = (current.url || "").toLowerCase();
  const text = (current.text || "").toLowerCase();
  return (
    url.includes(`/${ownerSlug}/${repoName}`) &&
    !url.includes(`/${repoName}/new/`) &&
    !url.includes("/new/") &&
    text.includes("hello world from")
  );
}

function passwordChangeSettledAfterSubmit(current) {
  const url = current.url || "";
  const visiblePasswordInputs = (current.passwordInputs || []).filter((item) => item.visible);
  const hasChangePasswordButton = (current.buttons || []).some((button) =>
    button.toLowerCase().includes("change password")
  );
  return (
    url.includes("github.com/settings/security") &&
    visiblePasswordInputs.length === 0 &&
    hasChangePasswordButton
  );
}

function loginOnlyComplete(current) {
  const url = current.url || "";
  const lower = (current.text || "").toLowerCase();
  const visiblePasswordInputs = (current.passwordInputs || []).filter((item) => item.visible);
  return (
    url.includes("github.com/settings/security") &&
    visiblePasswordInputs.length === 0 &&
    (lower.includes("account security") ||
      lower.includes("two-factor authentication") ||
      lower.includes("passkeys"))
  );
}

export {
  isGithubLoginPage,
  isGithubTwoFactorPrompt,
  requiresManualGithubStep,
  hasPasswordChangeForm,
  looksLikeSuccess,
  extractTotpCodeFrom2faFunValues,
  learningRepoContentForAccount,
  learningRepoDescriptionForAccount,
  learningRepoNameForAccount,
  learningRepoReadmeCommitted,
  loginOnlyComplete,
  passwordChangeSettledAfterSubmit,
  randomPostPasswordChangeDelayMs,
};

async function main() {
  if (!port) {
    console.error("GITHUB_DEVTOOLS_PORT is required");
    process.exit(2);
  }
  if (!githubLogin || !currentPassword || (!loginOnly && !newPassword)) {
    console.error("GitHub login, current password, and new password are required unless login-only mode is enabled");
    process.exit(2);
  }

  await openWebsocket();
  await send("Runtime.enable");
  await send("Page.enable");

  const deadline = Date.now() + timeoutSeconds * 1000;
  let lastAction = "started";
  let lastManualNoticeAt = 0;
  let submittedGithubCredentials = false;
  let submittedSudoPassword = false;
  let submittedPasswordChange = false;

  async function finishPasswordChange(message) {
    await navigate(settingsUrl);
    await sleep(1500);
    console.log(message);
    if (createLearningRepo) {
      const delayMs = randomPostPasswordChangeDelayMs();
      console.log(`Browser helper: waiting ${delayMs}ms before creating learning repository`);
      await sleep(delayMs);
      const repo = await createLearningRepository();
      console.log(`Browser helper: created learning repository ${repo.url}`);
    }
    ws.close();
    process.exit(0);
  }

  async function finishLoginOnly() {
    console.log("Browser helper: GitHub login-only reached security settings");
    ws.close();
    process.exit(0);
  }

  while (Date.now() < deadline) {
  const current = await state();
  const text = current.text || "";
  const lower = text.toLowerCase();
  const url = current.url || "";
  const passwordInputs = (current.passwordInputs || []).filter((item) => item.visible);

  if (looksLikeSuccess(lower)) {
    await finishPasswordChange("Browser helper: GitHub password change completed");
  }

  if (submittedPasswordChange && passwordChangeSettledAfterSubmit(current)) {
    await finishPasswordChange(
      "Browser helper: GitHub password change completed without explicit success message"
    );
  }

  if (!url.includes("github.com")) {
    await navigate(settingsUrl);
    lastAction = "navigated to GitHub settings";
    await sleep(1500);
    continue;
  }

  if (
    !submittedGithubCredentials &&
    isGithubLoginPage(url, lower) &&
    current.hasLoginInput &&
    passwordInputs.length >= 1
  ) {
    await setInput('#login_field,input[name="login"],input[name="user_login"],input[type="email"]', githubLogin);
    await setInput('#password,input[name="password"],input[type="password"]', currentPassword);
    await sleep(250);
    const clicked = (await clickText("Sign in")) || (await clickTextContaining("sign in"));
    submittedGithubCredentials = true;
    lastAction = `submitted GitHub credentials clicked=${clicked}`;
    console.log("Browser helper: submitted GitHub credentials");
    await sleep(3500);
    continue;
  }

  if (requiresManualGithubStep(url, lower)) {
    if (auto2faFun && totpSecret && isGithubTwoFactorPrompt(url, lower)) {
      const code = await codeFrom2faFun();
      if (await submitGithubTotpCode(code)) {
        console.log("Browser helper: submitted GitHub 2FA code from 2fa.fun");
        lastAction = "submitted GitHub 2FA code from 2fa.fun";
        await sleep(3500);
        continue;
      }
    }
    if (Date.now() - lastManualNoticeAt > 10_000) {
      console.log("Browser helper: GitHub verification/restriction detected; complete or inspect it manually");
      lastManualNoticeAt = Date.now();
    }
    lastAction = "waiting for manual GitHub verification";
    await sleep(2000);
    continue;
  }

  if (url.includes("github.com") && !url.includes("/settings/security")) {
    await navigate(settingsUrl);
    lastAction = "navigated to password settings";
    await sleep(1500);
    continue;
  }

  if (
    !submittedSudoPassword &&
    passwordInputs.length === 1 &&
    (lower.includes("confirm access") ||
      lower.includes("confirm password") ||
      lower.includes("sudo") ||
      lower.includes("verify your password"))
  ) {
    await setPasswordByIndex(0, currentPassword);
    await sleep(250);
    const submitted =
      (await clickText("Confirm")) ||
      (await clickTextContaining("confirm")) ||
      (await submitNearestPasswordForm());
    submittedSudoPassword = true;
    lastAction = `submitted sudo password submitted=${submitted}`;
    console.log("Browser helper: submitted GitHub sudo password");
    await sleep(2500);
    continue;
  }

  if (loginOnly && loginOnlyComplete(current)) {
    await finishLoginOnly();
  }

  if (!submittedPasswordChange && hasPasswordChangeForm(current)) {
    const oldSet =
      (await setInput('#user_old_password_sign_in_methods,#user_old_password,input[name="user[old_password]"],input[autocomplete="current-password"]', currentPassword)) ||
      (await setPasswordByIndex(0, currentPassword));
    const newSet =
      (await setInput('#user_new_password_sign_in_methods,#user_password,input[name="user[password]"],input[autocomplete="new-password"]', newPassword)) ||
      (await setPasswordByIndex(1, newPassword));
    const confirmSet =
      (await setInput('#user_confirm_new_password_sign_in_methods,#user_password_confirmation,input[name="user[password_confirmation]"]', newPassword)) ||
      (await setPasswordByIndex(2, newPassword));
    if (!oldSet || !newSet || !confirmSet) {
      lastAction = `password form detected but not all fields were set old=${oldSet} new=${newSet} confirm=${confirmSet}`;
      await sleep(1000);
      continue;
    }
    await sleep(250);
    const submitted =
      (await clickText("Update password")) ||
      (await clickText("Change password")) ||
      (await clickTextContaining("update password")) ||
      (await clickTextContaining("change password")) ||
      (await submitNearestPasswordForm());
    submittedPasswordChange = true;
    lastAction = `submitted password change old=${oldSet} new=${newSet} confirm=${confirmSet} submitted=${submitted}`;
    console.log("Browser helper: submitted GitHub password change form");
    await sleep(5000);
    continue;
  }

  if (
    !submittedPasswordChange &&
    current.buttons?.some((button) => button.toLowerCase().includes("change password"))
  ) {
    await clickTextContaining("change password");
    lastAction = "clicked change password";
    await sleep(1500);
    continue;
  }

  await sleep(1000);
}

  const finalState = await state();
  ws.close();
  console.error(
    `Browser helper timed out; lastAction=${lastAction}; title=${finalState.title}; url=${finalState.url}; text=${JSON.stringify((finalState.text || "").slice(0, 500))}`
  );
  process.exit(1);
}

if (isMain) {
  main().catch((error) => {
    console.error(error?.stack || error?.message || String(error));
    process.exit(1);
  });
}
