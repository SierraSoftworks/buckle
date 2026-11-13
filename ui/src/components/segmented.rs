//! A segmented toggle: two or three mutually exclusive views of one section
//! (Changed / All, Combined / By target), set in the section's head.

use yew::prelude::*;
use yewi::{Segment, SegmentedControl, Size, attrs};

#[derive(Properties, PartialEq)]
pub struct SegmentedProps {
    /// `(value, label)` per segment, in order.
    pub options: Vec<(AttrValue, AttrValue)>,
    pub value: AttrValue,
    pub on_change: Callback<AttrValue>,
    /// What the toggle switches, for assistive tech.
    pub label: AttrValue,
    #[prop_or_default]
    pub testid: Option<AttrValue>,
}

#[function_component(Segmented)]
pub fn segmented(props: &SegmentedProps) -> Html {
    html! {
        <SegmentedControl size={Size::Small} label={props.label.clone()} value={props.value.clone()}
            on_change={props.on_change.clone()}
            attrs={props.testid.clone().map(|t| attrs! { "data-testid" => t }).unwrap_or_default()}>
            { for props.options.iter().map(|(value, label)| html! {
                <Segment value={value.clone()}>{ label.clone() }</Segment>
            }) }
        </SegmentedControl>
    }
}
