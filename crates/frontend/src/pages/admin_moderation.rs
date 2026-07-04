//! Keyword moderation review console.
//!
//! Three tabs: **Keywords** (import via txt/json with category tagging, list,
//! delete), **Banned sessions** (list captured bans, inspect the full request
//! payload, keep or lift a ban), and **Categories** (manage the risk-category
//! taxonomy). Backed by the `/admin/llm-gateway/moderation/*` endpoints on the
//! cloud `llm-access` service.

use std::collections::BTreeMap;

use web_sys::{HtmlInputElement, HtmlSelectElement, HtmlTextAreaElement};
use yew::prelude::*;
use yew_router::prelude::Link;

use crate::{
    api::{
        add_admin_moderation_categories, add_admin_moderation_keywords,
        delete_admin_moderation_category, delete_admin_moderation_keyword,
        fetch_admin_moderation_banned_session, fetch_admin_moderation_banned_sessions,
        fetch_admin_moderation_categories, fetch_admin_moderation_keywords,
        review_admin_moderation_banned_session, AddAdminModerationCategoriesInput,
        AddAdminModerationCategoryInput, AddAdminModerationKeywordsInput,
        AdminModerationBannedSessionsResponse, AdminModerationCategoriesResponse,
        AdminModerationKeywordsResponse, ModerationBannedSessionDetailView, ModerationCategoryView,
        ReviewModerationBannedSessionInput,
    },
    components::tab_bar::render_tab_bar,
    pages::llm_access_shared::{confirm_destructive, format_timestamp_opt},
    router::Route,
};

const TAB_KEYWORDS: &str = "keywords";
const TAB_SESSIONS: &str = "sessions";
const TAB_CATEGORIES: &str = "categories";
const SESSIONS_PAGE_SIZE: usize = 50;

fn provider_badge(provider: &str) -> Classes {
    let base = classes!("rounded-full", "px-2", "py-1", "font-mono", "text-xs", "font-semibold");
    let color = if provider == "codex" {
        classes!("bg-sky-500/10", "text-sky-700", "dark:text-sky-200")
    } else {
        classes!("bg-violet-500/10", "text-violet-700", "dark:text-violet-200")
    };
    classes!(base, color)
}

/// Color a category badge by the category's severity.
fn severity_badge(severity: &str) -> Classes {
    let base = classes!("rounded-full", "px-2", "py-0.5", "font-mono", "text-[11px]");
    let color = match severity {
        "critical" => classes!("bg-red-500/15", "text-red-700", "dark:text-red-200"),
        "high" => classes!("bg-amber-500/15", "text-amber-700", "dark:text-amber-200"),
        "low" => classes!("bg-slate-500/10", "text-slate-600", "dark:text-slate-300"),
        _ => classes!("bg-indigo-500/10", "text-indigo-700", "dark:text-indigo-200"),
    };
    classes!(base, color)
}

/// Render the category codes for a keyword/ban as labeled, severity-colored
/// badges, resolving each code to its label + severity via `lookup`.
fn category_badges(codes: &[String], lookup: &BTreeMap<String, ModerationCategoryView>) -> Html {
    if codes.is_empty() {
        return html! { <span class={classes!("text-xs", "text-[var(--muted)]")}>{ "—" }</span> };
    }
    html! {
        <div class={classes!("flex", "flex-wrap", "gap-1")}>
            { for codes.iter().map(|code| {
                let (label, severity) = lookup
                    .get(code)
                    .map(|c| (c.label.clone(), c.severity.clone()))
                    .unwrap_or_else(|| (code.clone(), "medium".to_string()));
                html! { <span class={severity_badge(&severity)} title={code.clone()}>{ label }</span> }
            }) }
        </div>
    }
}

fn status_badge(status: &str) -> Classes {
    let base = classes!("rounded-full", "px-2", "py-1", "font-mono", "text-xs");
    let color = if status == "banned" {
        classes!("bg-red-500/10", "text-red-700", "dark:text-red-200")
    } else {
        classes!("bg-emerald-500/10", "text-emerald-700", "dark:text-emerald-200")
    };
    classes!(base, color)
}

fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| raw.to_string())
}

#[function_component(AdminModerationPage)]
pub fn admin_moderation_page() -> Html {
    let active_tab = use_state(|| TAB_KEYWORDS.to_string());
    let error = use_state(|| None::<String>);
    let flash = use_state(|| None::<String>);
    let refresh_tick = use_state(|| 0u64);

    // Category tab / shared lookup state.
    let categories = use_state(AdminModerationCategoriesResponse::default);
    let new_category_code = use_state(String::new);
    let new_category_label = use_state(String::new);
    let new_category_severity = use_state(|| "medium".to_string());

    // Keyword tab state.
    let keywords = use_state(AdminModerationKeywordsResponse::default);
    let keywords_loading = use_state(|| true);
    let import_content = use_state(String::new);
    let import_format = use_state(|| "txt".to_string());
    let import_note = use_state(String::new);
    let import_categories = use_state(Vec::<String>::new);
    let importing = use_state(|| false);

    // Banned session tab state.
    let sessions = use_state(AdminModerationBannedSessionsResponse::default);
    let sessions_loading = use_state(|| true);
    let session_status = use_state(|| "banned".to_string());
    let session_offset = use_state(|| 0usize);
    let selected_detail = use_state(|| None::<ModerationBannedSessionDetailView>);
    let detail_loading = use_state(|| false);

    let notify = {
        let flash = flash.clone();
        let error = error.clone();
        Callback::from(move |(message, is_error): (String, bool)| {
            if is_error {
                error.set(Some(message));
                flash.set(None);
            } else {
                flash.set(Some(message));
                error.set(None);
            }
        })
    };

    let reload = {
        let refresh_tick = refresh_tick.clone();
        Callback::from(move |_| refresh_tick.set((*refresh_tick).saturating_add(1)))
    };

    // Load categories (used by the Categories tab and by badge/import lookups).
    {
        let categories = categories.clone();
        let error = error.clone();
        let tick = *refresh_tick;
        use_effect_with(tick, move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_admin_moderation_categories().await {
                    Ok(response) => categories.set(response),
                    Err(message) => error.set(Some(message)),
                }
            });
            || ()
        });
    }

    // Code -> category lookup for rendering labeled, severity-colored badges.
    let category_lookup: BTreeMap<String, ModerationCategoryView> = categories
        .categories
        .iter()
        .map(|category| (category.code.clone(), category.clone()))
        .collect();

    // Load keywords.
    {
        let keywords = keywords.clone();
        let keywords_loading = keywords_loading.clone();
        let error = error.clone();
        let tick = *refresh_tick;
        use_effect_with(tick, move |_| {
            keywords_loading.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_admin_moderation_keywords().await {
                    Ok(response) => {
                        error.set(None);
                        keywords.set(response);
                    },
                    Err(message) => error.set(Some(message)),
                }
                keywords_loading.set(false);
            });
            || ()
        });
    }

    // Load banned sessions (re-runs on tick, status filter, or page change).
    {
        let sessions = sessions.clone();
        let sessions_loading = sessions_loading.clone();
        let error = error.clone();
        let status = (*session_status).clone();
        let offset = *session_offset;
        let deps = (*refresh_tick, status.clone(), offset);
        use_effect_with(deps, move |_| {
            sessions_loading.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_admin_moderation_banned_sessions(&status, SESSIONS_PAGE_SIZE, offset)
                    .await
                {
                    Ok(response) => {
                        error.set(None);
                        sessions.set(response);
                    },
                    Err(message) => error.set(Some(message)),
                }
                sessions_loading.set(false);
            });
            || ()
        });
    }

    let on_tab_click = {
        let active_tab = active_tab.clone();
        Callback::from(move |tab: String| active_tab.set(tab))
    };

    let on_import = {
        let import_content = import_content.clone();
        let import_format = import_format.clone();
        let import_note = import_note.clone();
        let import_categories = import_categories.clone();
        let importing = importing.clone();
        let notify = notify.clone();
        let reload = reload.clone();
        Callback::from(move |_| {
            if *importing {
                return;
            }
            let content = (*import_content).clone();
            if content.trim().is_empty() {
                notify.emit(("Keyword content is empty".to_string(), true));
                return;
            }
            let note = (*import_note).clone();
            let input = AddAdminModerationKeywordsInput {
                content,
                format: Some((*import_format).clone()),
                note: (!note.trim().is_empty()).then(|| note.trim().to_string()),
                categories: (*import_categories).clone(),
            };
            let importing = importing.clone();
            let notify = notify.clone();
            let reload = reload.clone();
            let import_content = import_content.clone();
            importing.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match add_admin_moderation_keywords(&input).await {
                    Ok(outcome) => {
                        notify.emit((
                            format!(
                                "Imported {} keyword(s), {} duplicate(s) skipped (parsed {})",
                                outcome.inserted, outcome.duplicates, outcome.parsed
                            ),
                            false,
                        ));
                        import_content.set(String::new());
                        reload.emit(());
                    },
                    Err(message) => notify.emit((message, true)),
                }
                importing.set(false);
            });
        })
    };

    let keywords_view = {
        let import_content = import_content.clone();
        let import_format = import_format.clone();
        let import_note = import_note.clone();
        let import_categories = import_categories.clone();
        let importing = importing.clone();
        let keywords = keywords.clone();
        let keywords_loading = keywords_loading.clone();
        let categories = categories.clone();
        let category_lookup = category_lookup.clone();
        let notify = notify.clone();
        let reload = reload.clone();

        let on_content_input = {
            let import_content = import_content.clone();
            Callback::from(move |e: InputEvent| {
                let target: HtmlTextAreaElement = e.target_unchecked_into();
                import_content.set(target.value());
            })
        };
        let on_format_change = {
            let import_format = import_format.clone();
            Callback::from(move |e: Event| {
                let target: HtmlSelectElement = e.target_unchecked_into();
                import_format.set(target.value());
            })
        };
        let on_note_input = {
            let import_note = import_note.clone();
            Callback::from(move |e: InputEvent| {
                let target: HtmlInputElement = e.target_unchecked_into();
                import_note.set(target.value());
            })
        };

        let stats = keywords.stats.clone();
        html! {
            <div class={classes!("space-y-4")}>
                <div class={classes!("grid", "gap-3", "sm:grid-cols-2", "lg:grid-cols-4")}>
                    { stat_card("Keywords", stats.keyword_count.to_string(), stats.loaded) }
                    { stat_card("Banned sessions", stats.banned_session_count.to_string(), stats.loaded) }
                    { stat_card("Suppressed hits", stats.suppressed_hit_count.to_string(), stats.loaded) }
                    { stat_card("Blocked requests", stats.blocked_requests_total.to_string(), stats.loaded) }
                </div>

                <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "p-5", "space-y-3")}>
                    <h3 class={classes!("font-mono", "text-xs", "uppercase", "tracking-[0.16em]", "text-[var(--muted)]")}>
                        { "Import keywords" }
                    </h3>
                    <textarea
                        class={classes!("w-full", "min-h-[8rem]", "rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "p-3", "font-mono", "text-sm")}
                        placeholder={"txt: one keyword (or space-separated phrase) per line\njson: [\"phrase a\", \"phrase b\"] or {\"keywords\": [...]}"}
                        value={(*import_content).clone()}
                        oninput={on_content_input}
                    />
                    <div class={classes!("flex", "flex-wrap", "items-center", "gap-3")}>
                        <select
                            class={classes!("rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
                            onchange={on_format_change}
                        >
                            <option value="txt" selected={*import_format == "txt"}>{ "Plain text (.txt)" }</option>
                            <option value="json" selected={*import_format == "json"}>{ "JSON" }</option>
                        </select>
                        <input
                            type="text"
                            class={classes!("flex-1", "min-w-[12rem]", "rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
                            placeholder="Optional note for this batch"
                            value={(*import_note).clone()}
                            oninput={on_note_input}
                        />
                        <button
                            type="button"
                            class={classes!("btn-terminal", "btn-terminal-primary")}
                            disabled={*importing}
                            onclick={on_import}
                        >
                            { if *importing { "Importing…" } else { "Import" } }
                        </button>
                    </div>
                    <div>
                        <div class={classes!("font-mono", "text-[11px]", "uppercase", "tracking-wider", "text-[var(--muted)]", "mb-1.5")}>
                            { "Tag imported keywords with categories" }
                        </div>
                        if categories.categories.is_empty() {
                            <div class={classes!("text-xs", "text-[var(--muted)]")}>
                                { "No categories yet — add some in the Categories tab." }
                            </div>
                        } else {
                            <div class={classes!("flex", "flex-wrap", "gap-2")}>
                                { for categories.categories.iter().map(|category| {
                                    let code = category.code.clone();
                                    let checked = import_categories.contains(&code);
                                    let on_toggle = {
                                        let import_categories = import_categories.clone();
                                        let code = code.clone();
                                        Callback::from(move |_| {
                                            let mut next = (*import_categories).clone();
                                            if let Some(pos) = next.iter().position(|c| c == &code) {
                                                next.remove(pos);
                                            } else {
                                                next.push(code.clone());
                                            }
                                            import_categories.set(next);
                                        })
                                    };
                                    html! {
                                        <label class={classes!("inline-flex", "items-center", "gap-1.5", "cursor-pointer", "rounded-lg", "border", "border-[var(--border)]", "px-2.5", "py-1")}>
                                            <input type="checkbox" checked={checked} onchange={on_toggle} />
                                            <span class={severity_badge(&category.severity)} title={category.code.clone()}>{ &category.label }</span>
                                        </label>
                                    }
                                }) }
                            </div>
                        }
                    </div>
                </div>

                <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "overflow-hidden")}>
                    if *keywords_loading {
                        <div class={classes!("p-5", "text-sm", "text-[var(--muted)]")}>{ "Loading keywords…" }</div>
                    } else if keywords.keywords.is_empty() {
                        <div class={classes!("p-5", "text-sm", "text-[var(--muted)]")}>{ "No keywords configured." }</div>
                    } else {
                        <div class={classes!("overflow-x-auto")}>
                            <table class={classes!("w-full", "min-w-[40rem]", "text-sm")}>
                                <thead>
                                    <tr class={classes!("border-b", "border-[var(--border)]", "text-left", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]")}>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Keyword" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Categories" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Source" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Note" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Added" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2", "text-right")}>{ "Actions" }</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    { for keywords.keywords.iter().map(|keyword| {
                                        let id = keyword.id;
                                        let keyword_text = keyword.keyword.clone();
                                        let on_delete = {
                                            let notify = notify.clone();
                                            let reload = reload.clone();
                                            let keyword_text = keyword_text.clone();
                                            Callback::from(move |_| {
                                                if !confirm_destructive(&format!("Delete keyword \"{keyword_text}\"?")) {
                                                    return;
                                                }
                                                let notify = notify.clone();
                                                let reload = reload.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    match delete_admin_moderation_keyword(id).await {
                                                        Ok(()) => {
                                                            notify.emit(("Keyword deleted".to_string(), false));
                                                            reload.emit(());
                                                        },
                                                        Err(message) => notify.emit((message, true)),
                                                    }
                                                });
                                            })
                                        };
                                        html! {
                                            <tr class={classes!("border-b", "border-[var(--border)]/50")}>
                                                <td class={classes!("px-4", "py-2", "font-mono", "break-all")}>{ &keyword.keyword }</td>
                                                <td class={classes!("px-4", "py-2")}>{ category_badges(&keyword.categories, &category_lookup) }</td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]")}>{ &keyword.source }</td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]")}>{ keyword.note.clone().unwrap_or_default() }</td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]", "whitespace-nowrap")}>{ format_timestamp_opt(Some(keyword.created_at_ms)) }</td>
                                                <td class={classes!("px-4", "py-2", "text-right")}>
                                                    <button type="button" class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")} onclick={on_delete}>
                                                        { "Delete" }
                                                    </button>
                                                </td>
                                            </tr>
                                        }
                                    }) }
                                </tbody>
                            </table>
                        </div>
                    }
                </div>
            </div>
        }
    };

    let sessions_view = {
        let sessions = sessions.clone();
        let sessions_loading = sessions_loading.clone();
        let session_status = session_status.clone();
        let session_offset = session_offset.clone();
        let selected_detail = selected_detail.clone();
        let detail_loading = detail_loading.clone();
        let category_lookup = category_lookup.clone();
        let notify = notify.clone();
        let reload = reload.clone();

        let on_status_change = {
            let session_status = session_status.clone();
            let session_offset = session_offset.clone();
            Callback::from(move |e: Event| {
                let target: HtmlSelectElement = e.target_unchecked_into();
                session_status.set(target.value());
                // Reset to the first page whenever the filter changes.
                session_offset.set(0);
            })
        };

        let offset = *session_offset;
        let on_prev = {
            let session_offset = session_offset.clone();
            Callback::from(move |_| {
                session_offset.set(offset.saturating_sub(SESSIONS_PAGE_SIZE));
            })
        };
        let on_next = {
            let session_offset = session_offset.clone();
            Callback::from(move |_| session_offset.set(offset + SESSIONS_PAGE_SIZE))
        };
        let page_start = if sessions.total == 0 { 0 } else { offset + 1 };
        let page_end = offset + sessions.sessions.len();

        html! {
            <div class={classes!("space-y-4")}>
                <div class={classes!("flex", "flex-wrap", "items-center", "gap-3")}>
                    <span class={classes!("font-mono", "text-xs", "uppercase", "tracking-[0.16em]", "text-[var(--muted)]")}>{ "Filter" }</span>
                    <select
                        class={classes!("rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
                        onchange={on_status_change}
                    >
                        <option value="banned" selected={*session_status == "banned"}>{ "Banned" }</option>
                        <option value="unbanned" selected={*session_status == "unbanned"}>{ "Unbanned" }</option>
                        <option value="all" selected={*session_status == "all"}>{ "All" }</option>
                    </select>
                    <span class={classes!("text-sm", "text-[var(--muted)]")}>
                        { format!("{page_start}–{page_end} of {} record(s)", sessions.total) }
                    </span>
                    <div class={classes!("ml-auto", "flex", "items-center", "gap-2")}>
                        <button
                            type="button"
                            class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")}
                            disabled={offset == 0}
                            onclick={on_prev}
                        >
                            { "‹ Prev" }
                        </button>
                        <button
                            type="button"
                            class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")}
                            disabled={!sessions.has_more}
                            onclick={on_next}
                        >
                            { "Next ›" }
                        </button>
                    </div>
                </div>

                <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "overflow-hidden")}>
                    if *sessions_loading {
                        <div class={classes!("p-5", "text-sm", "text-[var(--muted)]")}>{ "Loading banned sessions…" }</div>
                    } else if sessions.sessions.is_empty() {
                        <div class={classes!("p-5", "text-sm", "text-[var(--muted)]")}>{ "No banned sessions." }</div>
                    } else {
                        <div class={classes!("overflow-x-auto")}>
                            <table class={classes!("w-full", "min-w-[60rem]", "text-sm")}>
                                <thead>
                                    <tr class={classes!("border-b", "border-[var(--border)]", "text-left", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]")}>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Provider" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Key" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Matched keyword" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Categories" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Status" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Banned at" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2", "text-right")}>{ "Actions" }</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    { for sessions.sessions.iter().map(|session| {
                                        let id = session.id;
                                        let is_banned = session.status == "banned";

                                        let on_view = {
                                            let selected_detail = selected_detail.clone();
                                            let detail_loading = detail_loading.clone();
                                            let notify = notify.clone();
                                            Callback::from(move |_| {
                                                let selected_detail = selected_detail.clone();
                                                let detail_loading = detail_loading.clone();
                                                let notify = notify.clone();
                                                detail_loading.set(true);
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    match fetch_admin_moderation_banned_session(id).await {
                                                        Ok(detail) => selected_detail.set(Some(detail)),
                                                        Err(message) => notify.emit((message, true)),
                                                    }
                                                    detail_loading.set(false);
                                                });
                                            })
                                        };

                                        let on_toggle = {
                                            let notify = notify.clone();
                                            let reload = reload.clone();
                                            let selected_detail = selected_detail.clone();
                                            Callback::from(move |_| {
                                                let target_banned = !is_banned;
                                                let verb = if target_banned { "re-ban" } else { "unban" };
                                                if !confirm_destructive(&format!("Are you sure you want to {verb} this session?")) {
                                                    return;
                                                }
                                                let input = ReviewModerationBannedSessionInput {
                                                    banned: target_banned,
                                                    review_note: None,
                                                };
                                                let notify = notify.clone();
                                                let reload = reload.clone();
                                                let selected_detail = selected_detail.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    match review_admin_moderation_banned_session(id, &input).await {
                                                        Ok(_) => {
                                                            notify.emit((
                                                                if target_banned { "Session re-banned".to_string() } else { "Session unbanned".to_string() },
                                                                false,
                                                            ));
                                                            // Close the detail panel so its now-stale status is not shown.
                                                            if selected_detail.as_ref().is_some_and(|d| d.session.id == id) {
                                                                selected_detail.set(None);
                                                            }
                                                            reload.emit(());
                                                        },
                                                        Err(message) => notify.emit((message, true)),
                                                    }
                                                });
                                            })
                                        };

                                        html! {
                                            <tr class={classes!("border-b", "border-[var(--border)]/50")}>
                                                <td class={classes!("px-4", "py-2")}>
                                                    <span class={provider_badge(&session.provider)}>{ &session.provider }</span>
                                                </td>
                                                <td class={classes!("px-4", "py-2")}>
                                                    <div class={classes!("font-mono", "text-xs", "break-all")}>{ &session.key_name }</div>
                                                    <div class={classes!("font-mono", "text-[10px]", "text-[var(--muted)]", "break-all")}>{ &session.key_id }</div>
                                                </td>
                                                <td class={classes!("px-4", "py-2", "font-mono", "text-xs", "break-all")}>{ &session.matched_keyword }</td>
                                                <td class={classes!("px-4", "py-2")}>{ category_badges(&session.matched_categories, &category_lookup) }</td>
                                                <td class={classes!("px-4", "py-2")}>
                                                    <span class={status_badge(&session.status)}>{ &session.status }</span>
                                                </td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]", "whitespace-nowrap")}>{ format_timestamp_opt(Some(session.banned_at_ms)) }</td>
                                                <td class={classes!("px-4", "py-2", "text-right", "space-x-2", "whitespace-nowrap")}>
                                                    <button type="button" class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")} onclick={on_view}>
                                                        { "View" }
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class={classes!("btn-terminal", if is_banned { "btn-terminal-primary" } else { "" }, "!px-2.5", "!py-1.5", "!text-xs")}
                                                        onclick={on_toggle}
                                                    >
                                                        { if is_banned { "Unban" } else { "Re-ban" } }
                                                    </button>
                                                </td>
                                            </tr>
                                        }
                                    }) }
                                </tbody>
                            </table>
                        </div>
                    }
                </div>

                { detail_panel(&selected_detail, *detail_loading, &category_lookup) }
            </div>
        }
    };

    let categories_view = {
        let categories = categories.clone();
        let new_category_code = new_category_code.clone();
        let new_category_label = new_category_label.clone();
        let new_category_severity = new_category_severity.clone();
        let notify = notify.clone();
        let reload = reload.clone();

        let on_code_input = {
            let new_category_code = new_category_code.clone();
            Callback::from(move |e: InputEvent| {
                let target: HtmlInputElement = e.target_unchecked_into();
                new_category_code.set(target.value());
            })
        };
        let on_label_input = {
            let new_category_label = new_category_label.clone();
            Callback::from(move |e: InputEvent| {
                let target: HtmlInputElement = e.target_unchecked_into();
                new_category_label.set(target.value());
            })
        };
        let on_severity_change = {
            let new_category_severity = new_category_severity.clone();
            Callback::from(move |e: Event| {
                let target: HtmlSelectElement = e.target_unchecked_into();
                new_category_severity.set(target.value());
            })
        };
        let on_add_category = {
            let new_category_code = new_category_code.clone();
            let new_category_label = new_category_label.clone();
            let new_category_severity = new_category_severity.clone();
            let notify = notify.clone();
            let reload = reload.clone();
            Callback::from(move |_| {
                let code = (*new_category_code).clone();
                let label = (*new_category_label).clone();
                if code.trim().is_empty() || label.trim().is_empty() {
                    notify.emit(("Category code and label are required".to_string(), true));
                    return;
                }
                let input = AddAdminModerationCategoriesInput {
                    categories: vec![AddAdminModerationCategoryInput {
                        code: code.trim().to_string(),
                        label: label.trim().to_string(),
                        description: None,
                        severity: Some((*new_category_severity).clone()),
                    }],
                };
                let notify = notify.clone();
                let reload = reload.clone();
                let new_category_code = new_category_code.clone();
                let new_category_label = new_category_label.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match add_admin_moderation_categories(&input).await {
                        Ok(()) => {
                            notify.emit(("Category added".to_string(), false));
                            new_category_code.set(String::new());
                            new_category_label.set(String::new());
                            reload.emit(());
                        },
                        Err(message) => notify.emit((message, true)),
                    }
                });
            })
        };

        html! {
            <div class={classes!("space-y-4")}>
                <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "p-5", "space-y-3")}>
                    <h3 class={classes!("font-mono", "text-xs", "uppercase", "tracking-[0.16em]", "text-[var(--muted)]")}>
                        { "Add category" }
                    </h3>
                    <div class={classes!("flex", "flex-wrap", "items-center", "gap-3")}>
                        <input
                            type="text"
                            class={classes!("rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm", "font-mono")}
                            placeholder="code (a-z0-9_)"
                            value={(*new_category_code).clone()}
                            oninput={on_code_input}
                        />
                        <input
                            type="text"
                            class={classes!("flex-1", "min-w-[12rem]", "rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
                            placeholder="label"
                            value={(*new_category_label).clone()}
                            oninput={on_label_input}
                        />
                        <select
                            class={classes!("rounded-lg", "border", "border-[var(--border)]", "bg-[var(--bg)]", "px-3", "py-2", "text-sm")}
                            onchange={on_severity_change}
                        >
                            { for ["critical", "high", "medium", "low"].iter().map(|sev| html! {
                                <option value={*sev} selected={*new_category_severity == *sev}>{ *sev }</option>
                            }) }
                        </select>
                        <button type="button" class={classes!("btn-terminal", "btn-terminal-primary")} onclick={on_add_category}>
                            { "Add" }
                        </button>
                    </div>
                </div>

                <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "overflow-hidden")}>
                    if categories.categories.is_empty() {
                        <div class={classes!("p-5", "text-sm", "text-[var(--muted)]")}>{ "No categories configured." }</div>
                    } else {
                        <div class={classes!("overflow-x-auto")}>
                            <table class={classes!("w-full", "min-w-[40rem]", "text-sm")}>
                                <thead>
                                    <tr class={classes!("border-b", "border-[var(--border)]", "text-left", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]")}>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Category" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Code" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Severity" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2")}>{ "Description" }</th>
                                        <th scope="col" class={classes!("px-4", "py-2", "text-right")}>{ "Actions" }</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    { for categories.categories.iter().map(|category| {
                                        let code = category.code.clone();
                                        let on_delete = {
                                            let notify = notify.clone();
                                            let reload = reload.clone();
                                            let code = code.clone();
                                            Callback::from(move |_| {
                                                if !confirm_destructive(&format!("Delete category \"{code}\"?")) {
                                                    return;
                                                }
                                                let notify = notify.clone();
                                                let reload = reload.clone();
                                                let code = code.clone();
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    match delete_admin_moderation_category(&code).await {
                                                        Ok(()) => {
                                                            notify.emit(("Category deleted".to_string(), false));
                                                            reload.emit(());
                                                        },
                                                        Err(message) => notify.emit((message, true)),
                                                    }
                                                });
                                            })
                                        };
                                        html! {
                                            <tr class={classes!("border-b", "border-[var(--border)]/50")}>
                                                <td class={classes!("px-4", "py-2")}>
                                                    <span class={severity_badge(&category.severity)}>{ &category.label }</span>
                                                </td>
                                                <td class={classes!("px-4", "py-2", "font-mono", "text-xs")}>{ &category.code }</td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]")}>{ &category.severity }</td>
                                                <td class={classes!("px-4", "py-2", "text-[var(--muted)]")}>{ &category.description }</td>
                                                <td class={classes!("px-4", "py-2", "text-right")}>
                                                    <button type="button" class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")} onclick={on_delete}>
                                                        { "Delete" }
                                                    </button>
                                                </td>
                                            </tr>
                                        }
                                    }) }
                                </tbody>
                            </table>
                        </div>
                    }
                </div>
            </div>
        }
    };

    html! {
        <main class={classes!("min-h-screen", "bg-[var(--bg)]", "p-4")}>
            <div class={classes!("mx-auto", "max-w-7xl", "space-y-4")}>
                <div class={classes!("flex", "flex-wrap", "items-center", "justify-between", "gap-3")}>
                    <div>
                        <h1 class={classes!("text-xl", "font-semibold")}>{ "Keyword moderation" }</h1>
                        <p class={classes!("text-sm", "text-[var(--muted)]")}>
                            { "Block requests whose content matches banned keywords, and review flagged sessions." }
                        </p>
                    </div>
                    <div class={classes!("flex", "items-center", "gap-2")}>
                        <Link<Route> to={Route::AdminLlmGateway} classes={classes!("btn-terminal")}>
                            { "← LLM Gateway" }
                        </Link<Route>>
                        <button type="button" class={classes!("btn-terminal")} onclick={reload.reform(|_| ())}>
                            { "Refresh" }
                        </button>
                    </div>
                </div>

                if let Some(message) = (*flash).clone() {
                    <div class={classes!("rounded-lg", "bg-emerald-500/10", "px-3", "py-2", "text-sm", "text-emerald-700", "dark:text-emerald-200")}>
                        { message }
                    </div>
                }
                if let Some(err) = (*error).clone() {
                    <div class={classes!("rounded-lg", "bg-red-500/10", "px-3", "py-2", "text-sm", "text-red-700", "dark:text-red-200")}>
                        { err }
                    </div>
                }

                { render_tab_bar(
                    &active_tab,
                    &[
                        (TAB_KEYWORDS, "Keywords"),
                        (TAB_SESSIONS, "Banned sessions"),
                        (TAB_CATEGORIES, "Categories"),
                    ],
                    &on_tab_click,
                    None,
                ) }

                if *active_tab == TAB_KEYWORDS {
                    { keywords_view }
                } else if *active_tab == TAB_SESSIONS {
                    { sessions_view }
                } else {
                    { categories_view }
                }
            </div>
        </main>
    }
}

fn stat_card(label: &str, value: String, loaded: bool) -> Html {
    html! {
        <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "p-4")}>
            <div class={classes!("font-mono", "text-xs", "uppercase", "tracking-[0.16em]", "text-[var(--muted)]")}>{ label.to_string() }</div>
            <div class={classes!("mt-1", "text-2xl", "font-semibold")}>{ if loaded { value } else { "…".to_string() } }</div>
        </div>
    }
}

fn detail_panel(
    detail: &UseStateHandle<Option<ModerationBannedSessionDetailView>>,
    loading: bool,
    category_lookup: &BTreeMap<String, ModerationCategoryView>,
) -> Html {
    if loading {
        return html! {
            <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "p-5", "text-sm", "text-[var(--muted)]")}>
                { "Loading captured request…" }
            </div>
        };
    }
    let Some(detail_value) = (**detail).clone() else {
        return Html::default();
    };
    let on_close = {
        let detail = detail.clone();
        Callback::from(move |_| detail.set(None))
    };
    let detail = detail_value;
    let session = &detail.session;
    html! {
        <div class={classes!("rounded-xl", "border", "border-[var(--border)]", "bg-[var(--surface)]", "p-5", "space-y-3")}>
            <div class={classes!("flex", "flex-wrap", "items-center", "justify-between", "gap-2")}>
                <h3 class={classes!("font-mono", "text-sm", "font-semibold")}>
                    { format!("Captured request · session #{}", session.id) }
                </h3>
                <div class={classes!("flex", "items-center", "gap-2")}>
                    <span class={status_badge(&session.status)}>{ &session.status }</span>
                    <button
                        type="button"
                        class={classes!("btn-terminal", "!px-2.5", "!py-1.5", "!text-xs")}
                        title="Close"
                        aria-label="Close captured request"
                        onclick={on_close}
                    >
                        { "✕" }
                    </button>
                </div>
            </div>
            <div class={classes!("grid", "gap-2", "sm:grid-cols-2", "text-sm")}>
                { detail_field("Provider", &session.provider) }
                { detail_field("Endpoint", &session.endpoint) }
                { detail_field("Model", &session.model) }
                { detail_field("Client IP", &session.client_ip) }
                { detail_field("Session id", &session.session_id) }
                { detail_field("Matched keyword", &session.matched_keyword) }
                { detail_field("Match range", &format!("{}..{}", session.match_start, session.match_end)) }
            </div>
            <div class={classes!("flex", "items-center", "gap-2")}>
                <span class={classes!("font-mono", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]")}>{ "Categories:" }</span>
                { category_badges(&session.matched_categories, category_lookup) }
            </div>
            if !session.matched_context.is_empty() {
                <div>
                    <div class={classes!("font-mono", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]", "mb-1")}>{ "Matched context" }</div>
                    <pre class={classes!("rounded-lg", "bg-[var(--bg)]", "p-3", "text-xs", "whitespace-pre-wrap", "break-words")}>{ &session.matched_context }</pre>
                </div>
            }
            <div>
                <div class={classes!("font-mono", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]", "mb-1")}>{ "Request headers" }</div>
                <pre class={classes!("max-h-64", "overflow-auto", "rounded-lg", "bg-[var(--bg)]", "p-3", "text-xs")}>{ pretty_json(&detail.request_headers_json) }</pre>
            </div>
            <div>
                <div class={classes!("font-mono", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]", "mb-1")}>{ "Request body" }</div>
                <pre class={classes!("max-h-96", "overflow-auto", "rounded-lg", "bg-[var(--bg)]", "p-3", "text-xs")}>{ pretty_json(&detail.request_body_json) }</pre>
            </div>
        </div>
    }
}

fn detail_field(label: &str, value: &str) -> Html {
    html! {
        <div>
            <span class={classes!("font-mono", "text-xs", "uppercase", "tracking-wider", "text-[var(--muted)]")}>{ label.to_string() }{ ": " }</span>
            <span class={classes!("font-mono", "break-all")}>{ if value.is_empty() { "-" } else { value } }</span>
        </div>
    }
}
