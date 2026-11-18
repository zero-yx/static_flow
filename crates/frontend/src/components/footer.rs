use yew::prelude::*;

use crate::i18n::current::{common as common_text, footer as t};

#[function_component(Footer)]
pub fn footer() -> Html {
    html! {
        <footer class={classes!(
            "bg-[var(--surface-alt)]",
            "dark:bg-[var(--surface)]",
            "border-t", "border-[var(--border)]",
            "mt-8",
            "py-12",
            "text-center",
            "text-sm",
            "text-[var(--muted)]"
        )}>
            <div class={classes!(
                "max-w-7xl",
                "mx-auto",
                "flex",
                "flex-col",
                "items-center",
                "justify-center",
                "gap-6",
                "px-4"
            )}>
                <div class={classes!("font-medium", "tracking-wide")}>
                    {t::COPYRIGHT}
                </div>
                <div class={classes!("flex", "items-center", "gap-4")} aria-label={t::SOCIAL_ARIA}>
                    <a
                        class={classes!(
                            "inline-flex",
                            "h-[var(--hit-size)]",
                            "w-[var(--hit-size)]",
                            "items-center",
                            "justify-center",
                            "rounded-lg",
                            "text-[var(--primary)]",
                            "transition-colors",
                            "duration-100",
                            "hover:bg-[var(--surface-alt)]",
                            "hover:text-[var(--primary)]"
                        )}
                        href="https://github.com/zero-yx"
                        target="_blank"
                        rel="noopener noreferrer"
                        aria-label={common_text::GITHUB}
                    >
                        <i class="fa-brands fa-github-alt" aria-hidden="true"></i>
                        <span class={classes!("sr-only")}>{ common_text::GITHUB }</span>
                    </a>

                </div>
                // 备案号占位，备案完成后可恢复下面的节点
                // <a
                //     class="text-[var(--primary)] hover:underline transition-colors duration-100"
                //     href="https://beian.miit.gov.cn/"
                //     target="_blank"
                //     rel="noopener noreferrer"
                // >
                //     {"粤ICP备xxxxxxx号"}
                // </a>
            </div>
        </footer>
    }
}
