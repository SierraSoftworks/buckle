//! Every view's actions as one split button: the action an operator most
//! likely wants on its face, the rest behind the chevron, in sections when
//! they are of different kinds. Callers list the actions most likely first;
//! the first is the face. Drawn by yewi's split button and menu.

use yew::prelude::*;

use yewi::{MenuGroup, Size, attrs};

use super::ui::Kind;

/// One action of a [`SplitButton`].
#[derive(Clone, PartialEq)]
pub struct MenuItem {
    pub label: AttrValue,
    /// A muted suffix that says where the action comes from (its module).
    pub aside: Option<AttrValue>,
    /// The menu section the action is listed under; a run of items with the
    /// same section is one group, headed by it when it has a name.
    pub section: Option<AttrValue>,
    /// How the face looks when this is the face; in the menu only `Danger`
    /// (red) and `Caution` (orange) show.
    pub kind: Kind,
    pub title: Option<AttrValue>,
    pub onclick: Callback<MouseEvent>,
}

impl MenuItem {
    pub fn new(label: impl Into<AttrValue>, onclick: Callback<MouseEvent>) -> Self {
        Self {
            label: label.into(),
            aside: None,
            section: None,
            kind: Kind::Default,
            title: None,
            onclick,
        }
    }

    pub fn kind(self, kind: Kind) -> Self {
        Self { kind, ..self }
    }

    pub fn danger(self) -> Self {
        self.kind(Kind::Danger)
    }

    pub fn aside(self, aside: impl Into<AttrValue>) -> Self {
        Self {
            aside: Some(aside.into()),
            ..self
        }
    }

    pub fn section(self, section: impl Into<AttrValue>) -> Self {
        Self {
            section: Some(section.into()),
            ..self
        }
    }

    pub fn title(self, title: impl Into<AttrValue>) -> Self {
        Self {
            title: Some(title.into()),
            ..self
        }
    }

}

#[derive(Properties, PartialEq)]
pub struct SplitButtonProps {
    /// The actions, most likely first: the first is the face. Nothing
    /// renders when there are none, and no chevron when there is one.
    pub items: Vec<MenuItem>,
    /// What the chevron opens, for a screen reader.
    #[prop_or(AttrValue::Static("More actions"))]
    pub menu_label: AttrValue,
    #[prop_or_default]
    pub small: bool,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

/// The menu's items in groups of one section each, in order.
fn groups(items: &[MenuItem]) -> Vec<(Option<AttrValue>, &[MenuItem])> {
    let mut out: Vec<(Option<AttrValue>, &[MenuItem])> = Vec::new();
    let mut start = 0;
    for i in 1..=items.len() {
        if i == items.len() || items[i].section != items[start].section {
            out.push((items[start].section.clone(), &items[start..i]));
            start = i;
        }
    }
    out
}

#[function_component(SplitButton)]
pub fn split_button(props: &SplitButtonProps) -> Html {
    let Some((face, rest)) = props.items.split_first() else {
        return html! {};
    };
    let mut extra = Vec::new();
    if let Some(t) = &props.testid {
        extra.push(("data-testid", t.clone()));
    }
    if let Some(t) = &face.title {
        extra.push(("title", t.clone()));
    }
    let item = |item: &MenuItem| {
        html! {
            <yewi::MenuItem on_select={item.onclick.clone()}
                tone={match item.kind {
                    Kind::Danger => yewi::Tone::Danger,
                    Kind::Caution => yewi::Tone::Warn,
                    _ => yewi::Tone::Neutral,
                }}
                class={classes!((item.kind == Kind::Caution).then_some("menu-item--caution"))}
                aside={item.aside.clone().map(|a| html! { <>{ " " }{ a }</> }).unwrap_or_default()}
                attrs={item.title.clone().map(|t| attrs! { "title" => t }).unwrap_or_default()}>
                { item.label.clone() }
            </yewi::MenuItem>
        }
    };
    html! {
        <yewi::SplitButton class={classes!("split", face.kind.class())} variant={face.kind.variant()}
            size={if props.small { Size::Small } else { Size::Medium }}
            label={face.label.clone()} menu_label={props.menu_label.clone()}
            on_click={face.onclick.clone()} attrs={yewi::Attrs::new(extra)}>
            { for groups(rest).into_iter().map(|(section, items)| html! {
                <MenuGroup label={section}>{ for items.iter().map(item) }</MenuGroup>
            }) }
        </yewi::SplitButton>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &'static str, section: Option<&'static str>) -> MenuItem {
        let item = MenuItem::new(label, Callback::noop());
        match section {
            Some(s) => item.section(s),
            None => item,
        }
    }

    #[test]
    fn a_menu_groups_each_run_of_one_section() {
        let items = [
            item("Pause…", None),
            item("Reboot…", Some("Actions")),
            item("Restart", Some("Actions")),
            item("Forget…", None),
        ];
        let got: Vec<(Option<String>, Vec<String>)> = groups(&items)
            .into_iter()
            .map(|(s, items)| {
                (
                    s.map(|s| s.to_string()),
                    items.iter().map(|i| i.label.to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (None, vec!["Pause…".to_string()]),
                (
                    Some("Actions".to_string()),
                    vec!["Reboot…".to_string(), "Restart".to_string()]
                ),
                (None, vec!["Forget…".to_string()]),
            ]
        );
        assert!(groups(&[]).is_empty());
    }
}
