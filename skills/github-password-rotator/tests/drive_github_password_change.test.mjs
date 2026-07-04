import test from "node:test";
import assert from "node:assert/strict";

import {
  extractTotpCodeFrom2faFunValues,
  isGithubTwoFactorPrompt,
  learningRepoContentForAccount,
  learningRepoReadmeCommitted,
  learningRepoNameForAccount,
  passwordChangeSettledAfterSubmit,
  randomPostPasswordChangeDelayMs,
  requiresManualGithubStep,
} from "../scripts/drive_github_password_change.mjs";

test("does not treat the account security settings page as manual verification", () => {
  const settingsText = [
    "Account security",
    "Password",
    "Two-factor authentication",
    "Passkeys",
    "Security keys",
  ].join("\n").toLowerCase();

  assert.equal(
    requiresManualGithubStep("https://github.com/settings/security", settingsText),
    false,
  );
});

test("still detects GitHub two-factor challenge pages", () => {
  assert.equal(
    requiresManualGithubStep(
      "https://github.com/sessions/two-factor",
      "two-factor authentication code",
    ),
    true,
  );
});

test("detects GitHub two-factor checkup pages", () => {
  assert.equal(
    isGithubTwoFactorPrompt(
      "https://github.com/settings/two_factor_checkup?",
      "verify your recently configured two-factor authentication method",
    ),
    true,
  );
});

test("treats collapsed password form after submit as settled without a success flash", () => {
  assert.equal(
    passwordChangeSettledAfterSubmit({
      url: "https://github.com/settings/security",
      passwordInputs: [],
      buttons: ["Change password"],
    }),
    true,
  );
});

test("does not treat a visible password form as settled after submit", () => {
  assert.equal(
    passwordChangeSettledAfterSubmit({
      url: "https://github.com/settings/security",
      passwordInputs: [
        { visible: true },
        { visible: true },
        { visible: true },
      ],
      buttons: ["Update password"],
    }),
    false,
  );
});

test("builds a deterministic learning repository name from the account name", () => {
  assert.equal(learningRepoNameForAccount("Thompsonx"), "hello-world-from-thompsonx");
  assert.equal(learningRepoNameForAccount("Jane_Doe.42"), "hello-world-from-jane-doe-42");
});

test("waits 3 to 10 seconds after password change before repository creation", () => {
  assert.equal(randomPostPasswordChangeDelayMs(() => 0), 3000);
  assert.equal(randomPostPasswordChangeDelayMs((max) => max - 1), 10000);
});

test("builds varied beginner README content for the same account", () => {
  const first = learningRepoContentForAccount("alice", () => 0);
  const second = learningRepoContentForAccount("alice", (length) => Math.min(1, length - 1));

  assert.match(first, /^# Hello World from alice/m);
  assert.match(first, /beginner|practice|learning/i);
  assert.notEqual(first, second);
});

test("does not treat the new-file editor as a committed learning README", () => {
  assert.equal(
    learningRepoReadmeCommitted(
      {
        url: "https://github.com/alice/hello-world-from-alice/new/main?filename=README.md",
        text: "# Hello World from alice",
      },
      "alice",
      "hello-world-from-alice",
    ),
    false,
  );
});

test("treats repository file view as a committed learning README", () => {
  assert.equal(
    learningRepoReadmeCommitted(
      {
        url: "https://github.com/alice/hello-world-from-alice/tree/main",
        text: "README Hello World from alice",
      },
      "alice",
      "hello-world-from-alice",
    ),
    true,
  );
});

test("extracts 2fa.fun code from OTP input values", () => {
  assert.equal(
    extractTotpCodeFrom2faFunValues(["", "123456"]),
    "123456",
  );
});
