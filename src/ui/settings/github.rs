//! The GitHub group on the General page.

use super::*;

impl Tty7App {
    pub(super) fn render_github_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = cx.global::<Config>().github_graphite_links;
        let graphite = self.settings_switch("wt-github-graphite", on, cx, |this, on, _, cx| {
            this.set_github_graphite_links(on, cx)
        });
        self.settings_group(
            Some(t(L10nKey::PanelGitHubTitle)),
            None,
            [self
                .settings_row(
                    t(L10nKey::SettingsGitHubGraphiteLinks),
                    t(L10nKey::SettingsGitHubGraphiteLinksDesc),
                    graphite,
                    cx,
                )
                .into_any_element()],
            cx,
        )
    }

    pub(crate) fn set_github_graphite_links(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.github_graphite_links = on);
    }
}
