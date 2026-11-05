//! A segmented toggle: two or three mutually exclusive views of one card
//! (Changed / All, Combined / By target), set in the card's head.

use yew::prelude::*;

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
        <div class="segmented" role="radiogroup" aria-label={props.label.clone()} data-testid={props.testid.clone()}>
            { for props.options.iter().map(|(value, label)| {
                let on = *value == props.value;
                let onclick = {
                    let (cb, v) = (props.on_change.clone(), value.clone());
                    Callback::from(move |_: MouseEvent| if !on { cb.emit(v.clone()) })
                };
                html! {
                    <button type="button" role="radio" aria-checked={on.to_string()}
                        class={classes!("segmented__option", on.then_some("segmented__option--on"))}
                        {onclick}>
                        { label.clone() }
                    </button>
                }
            }) }
        </div>
    }
}
