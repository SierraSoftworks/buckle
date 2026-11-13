//! The app bar's search box. What it searches is the page's business: each
//! page that can be searched reads its query from the URL (`?q=`, or the
//! audit log's `?filter=`), so a search can be linked to, and this box is
//! how that parameter is typed. Pages with nothing to search show no box.

use std::collections::BTreeMap;
use std::rc::Rc;

use yew::prelude::*;
use yew_router::prelude::*;
use yewi::{QueryField, QueryInput, QuerySyntax};

use crate::app::Route;
use crate::hooks::use_debounced;
use crate::logic::filter::{AUDIT_FIELDS, Check, check};

/// What the box does on one page.
struct Search {
    /// The query parameter it writes.
    param: &'static str,
    label: &'static str,
    placeholder: &'static str,
    /// A filt-rs expression, checked before it is applied, rather than
    /// words to match.
    expression: bool,
}

fn search(route: &Route) -> Option<Search> {
    let words = |label, placeholder| Search {
        param: "q",
        label,
        placeholder,
        expression: false,
    };
    match route {
        Route::Hosts => Some(words("Search hosts", "Search hosts, labels, groups")),
        Route::Workloads => Some(words("Search workloads", "Search workloads, clusters")),
        Route::Clusters => Some(words("Search clusters", "Search clusters, labels, groups")),
        Route::Audit => Some(Search {
            param: "filter",
            label: "Filter the audit log",
            placeholder: r#"Filter: actor == "alice" && action startswith "rollout.""#,
            expression: true,
        }),
        _ => None,
    }
}

/// Whether the page at `route` is searched from the app bar.
pub fn searchable(route: &Route) -> bool {
    search(route).is_some()
}

/// What each audit field holds, for the box's suggestions.
fn audit_fields() -> Rc<[QueryField]> {
    AUDIT_FIELDS
        .iter()
        .map(|name| {
            let field = QueryField::new(*name);
            match *name {
                "at" => field.description("When it happened: at > now() - 1h"),
                "actor" => field.description("Who did it; controller and system are Buckle"),
                "action" => field.description("What was done: rollout.approve, host.forget, …"),
                "outcome" => field
                    .description("How it went")
                    .values(["success", "failure", "denied"].map(AttrValue::from)),
                "subject" => field.description("The host, cluster, rollout or revision"),
                "message" => field.description("The entry's text"),
                "trace_id" => field.description("The trace it was recorded under"),
                "progress" => field
                    .description("A rollout's ordinary steps")
                    .values(["true", "false"].map(AttrValue::from)),
                "detail.*" => field.description("A property of the entry's detail"),
                _ => field,
            }
        })
        .collect()
}

/// The page's query parameters, as the URL has them.
type Params = BTreeMap<String, String>;

#[function_component(AppSearch)]
pub fn app_search() -> Html {
    let route = use_route::<Route>();
    let location = use_location();
    let navigator = use_navigator();
    let spec = route.as_ref().and_then(search);
    let params: Params = location
        .and_then(|l| l.query::<Params>().ok())
        .unwrap_or_default();
    let applied = spec
        .as_ref()
        .and_then(|s| params.get(s.param).cloned())
        .unwrap_or_default();
    let expression = spec.as_ref().is_some_and(|s| s.expression);

    let draft = use_state(|| applied.clone());
    let settled = use_debounced((*draft).clone(), if expression { 250 } else { 120 });
    // What this box last wrote to the URL: the URL changing to it is not
    // news, and must not overwrite what has been typed since.
    let written = use_mut_ref(|| None::<String>);

    // The URL follows the box once the text is worth applying.
    {
        let (route, params, applied, written) = (
            route.clone(),
            params.clone(),
            applied.clone(),
            written.clone(),
        );
        let param = spec.as_ref().map(|s| s.param);
        use_effect_with(settled.clone(), move |text| {
            let ok = !expression
                || matches!(check(text), Check::Valid | Check::Unchecked | Check::Empty);
            if let (true, Some(param), Some(route), Some(n)) =
                (ok && *text != applied, param, route, navigator)
            {
                let mut params = params;
                if text.is_empty() {
                    params.remove(param);
                } else {
                    params.insert(param.to_string(), text.clone());
                }
                *written.borrow_mut() = Some(text.clone());
                let _ = n.replace_with_query(&route, &params);
            }
            || ()
        });
    }
    // And the box follows the URL: a link, the back button, another page, or
    // a filter the page itself applied.
    {
        let draft = draft.clone();
        use_effect_with((route.clone(), applied.clone()), move |(_, applied)| {
            if written.borrow_mut().take().as_ref() != Some(applied) && *draft != *applied {
                draft.set(applied.clone());
            }
            || ()
        });
    }

    let fields = use_memo(expression, |expression| {
        if *expression {
            audit_fields()
        } else {
            Rc::from([])
        }
    });
    let Some(spec) = spec else {
        return html! {};
    };
    let error = match (expression, check(&draft)) {
        (true, Check::Invalid { message, advice }) => Some(AttrValue::from(if advice.is_empty() {
            message
        } else {
            format!("{message} {}", advice.join(" "))
        })),
        _ => None,
    };
    let on_input = {
        let draft = draft.clone();
        Callback::from(move |text: String| draft.set(text))
    };
    html! {
        <QueryInput class="shell__search" label={spec.label} placeholder={spec.placeholder}
            value={(*draft).clone()} {on_input} {error} fields={(*fields).clone()}
            syntax={if expression { QuerySyntax::Expression } else { QuerySyntax::Terms }}
            mono={expression} />
    }
}
