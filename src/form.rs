//! Form submission. GET and urlencoded POST are supported; file uploads are not.
//! Forms with password fields are refused: without cookies a login cannot work anyway,
//! and with GET the password would end up in the URL, history and server logs.

use std::collections::HashMap;

use url::Url;

use crate::dom::{Element, Node};

/// What to fetch when a form is submitted.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Get(Url),
    /// urlencoded POST; the body is sent once and never replayed from history.
    Post(Url, Vec<(String, String)>),
}

impl Request {
    pub fn url(&self) -> &Url {
        match self {
            Request::Get(url) | Request::Post(url, _) => url,
        }
    }
}

/// Builds the request for submitting the form that contains `control` (a text field or a
/// submit button, identified by `Element::id`). `values` holds what the user typed.
/// Returns None when the control is not inside a form.
pub fn submit(dom: &Node, base: &Url, control: usize, values: &HashMap<usize, String>) -> Option<Result<Request, String>> {
    let mut ancestors = Vec::new();
    find(dom, control, &mut ancestors)?;
    let form = ancestors.iter().rev().find(|e| e.tag == "form")?;
    Some(build(form, base, control, values))
}

fn build(form: &Element, base: &Url, control: usize, values: &HashMap<usize, String>) -> Result<Request, String> {
    let method = form.attr("method").unwrap_or("get").trim().to_ascii_lowercase();
    let enctype = form.attr("enctype").unwrap_or("").trim().to_ascii_lowercase();
    if method == "post" && enctype == "multipart/form-data" {
        return Err("This form uploads files, which SafeBrowser does not support.".into());
    }
    let mut pairs = Vec::new();
    collect(form, control, values, &mut pairs)?;

    let mut url = match form.attr("action").map(str::trim).filter(|a| !a.is_empty()) {
        Some(action) => base.join(action).map_err(|_| "The form's action is not a valid URL.".to_string())?,
        None => base.clone(),
    };
    url.set_fragment(None);
    if method == "post" {
        return Ok(Request::Post(url, pairs));
    }
    // GET replaces the action's query with the form data
    url.query_pairs_mut().clear().extend_pairs(&pairs);
    Ok(Request::Get(url))
}

/// The ids of the radio buttons that share a name (and form) with the radio `id`, including itself.
pub fn radio_group(dom: &Node, id: usize) -> Vec<usize> {
    let mut ancestors = Vec::new();
    let Some(radio) = find(dom, id, &mut ancestors) else { return vec![id] };
    let Some(name) = radio.attr("name").filter(|n| !n.is_empty()) else { return vec![id] };
    let scope: &Element = ancestors.iter().rev().find(|e| e.tag == "form").copied().unwrap_or(match dom {
        Node::Element(root) => root,
        _ => return vec![id],
    });
    let mut out = Vec::new();
    gather_radios(scope, name, &mut out);
    out
}

fn gather_radios(el: &Element, name: &str, out: &mut Vec<usize>) {
    for child in &el.children {
        let Node::Element(c) = child else { continue };
        if c.tag == "input" && c.attr("type").is_some_and(|t| t.trim().eq_ignore_ascii_case("radio")) && c.attr("name") == Some(name) {
            out.push(c.id());
        }
        gather_radios(c, name, out);
    }
}

/// Finds the element with `id`, leaving its ancestors (root first) in `path`.
fn find<'a>(node: &'a Node, id: usize, path: &mut Vec<&'a Element>) -> Option<&'a Element> {
    let Node::Element(el) = node else { return None };
    if el.id() == id {
        return Some(el);
    }
    path.push(el);
    for child in &el.children {
        if let Some(found) = find(child, id, path) {
            return Some(found);
        }
    }
    path.pop();
    None
}

/// Collects (name, value) pairs of the form's controls in document order.
fn collect(el: &Element, control: usize, values: &HashMap<usize, String>, out: &mut Vec<(String, String)>) -> Result<(), String> {
    for child in &el.children {
        let Node::Element(c) = child else { continue };
        let name = c.attr("name").filter(|n| !n.is_empty()).map(str::to_string);
        let typed = || values.get(&c.id()).cloned();
        if c.attr("disabled").is_none() {
            match c.tag.as_str() {
                "input" => {
                    let kind = c.attr("type").unwrap_or("text").trim().to_ascii_lowercase();
                    match kind.as_str() {
                        "password" => {
                            return Err("This form has a password field. SafeBrowser does not send passwords (it has no cookies, so logins cannot work).".into());
                        }
                        "checkbox" | "radio" => {
                            let checked = values.get(&c.id()).map_or(c.attr("checked").is_some(), |v| v == "1");
                            if let (Some(name), true) = (name, checked) {
                                out.push((name, c.attr("value").unwrap_or("on").to_string()));
                            }
                        }
                        // Only the button that was pressed is submitted
                        "submit" | "image" => {
                            if let (Some(name), true) = (name, c.id() == control) {
                                out.push((name, c.attr("value").unwrap_or("").to_string()));
                            }
                        }
                        "button" | "reset" | "file" => {}
                        _ => {
                            if let Some(name) = name {
                                out.push((name, typed().unwrap_or_else(|| c.attr("value").unwrap_or("").to_string())));
                            }
                        }
                    }
                }
                "textarea" => {
                    if let Some(name) = name {
                        out.push((name, typed().unwrap_or_else(|| c.text())));
                    }
                }
                "select" => {
                    let options = c.options();
                    let chosen = values.get(&c.id()).and_then(|v| v.parse::<usize>().ok()).and_then(|i| options.get(i).copied());
                    if let (Some(name), Some(option)) = (name, chosen.or_else(|| c.selected_option())) {
                        out.push((name, option.option_value()));
                    }
                }
                "button" => {
                    let kind = c.attr("type").unwrap_or("submit").to_ascii_lowercase();
                    if let (Some(name), true) = (name, kind == "submit" && c.id() == control) {
                        out.push((name, c.attr("value").unwrap_or("").to_string()));
                    }
                }
                _ => {}
            }
        }
        if !matches!(c.tag.as_str(), "select" | "textarea") {
            collect(c, control, values, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn element_by_name<'a>(node: &'a Node, name: &str) -> Option<&'a Element> {
        let Node::Element(el) = node else { return None };
        if el.attr("name") == Some(name) {
            return Some(el);
        }
        el.children.iter().find_map(|c| element_by_name(c, name))
    }

    fn base() -> Url {
        Url::parse("https://example.com/page?x=1#top").unwrap()
    }

    #[test]
    fn get_form_builds_query_from_typed_and_default_values() {
        let dom = html::parse(
            "<form action=/search><input name=q value=old><input type=hidden name=lang value=en>
             <input type=checkbox name=safe checked><input type=checkbox name=skip>
             <select name=n><option value=1>one<option value=2 selected>two</select>
             <input type=submit name=go value=Go></form>",
        );
        let q = element_by_name(&dom, "q").unwrap().id();
        let go = element_by_name(&dom, "go").unwrap().id();
        let values = HashMap::from([(q, "rust & safety".to_string())]);
        let req = submit(&dom, &base(), go, &values).unwrap().unwrap();
        assert_eq!(req, Request::Get(Url::parse("https://example.com/search?q=rust+%26+safety&lang=en&safe=on&n=2&go=Go").unwrap()));

        // Pressing Enter in the field submits without the button's name
        let req = submit(&dom, &base(), q, &values).unwrap().unwrap();
        assert!(!req.url().as_str().contains("go="));
    }

    #[test]
    fn toggled_controls_override_defaults() {
        let dom = html::parse(
            "<form action=/s><input type=checkbox name=a checked><input type=checkbox name=b>
             <input type=radio name=r value=1 checked><input type=radio name=r value=2>
             <select name=n><option value=1>one<option value=2 selected>two</select><input type=submit name=go></form>",
        );
        let a = element_by_name(&dom, "a").unwrap().id();
        let b = element_by_name(&dom, "b").unwrap().id();
        let n = element_by_name(&dom, "n").unwrap().id();
        let go = element_by_name(&dom, "go").unwrap().id();
        let r1 = element_by_name(&dom, "r").unwrap().id();
        let group = radio_group(&dom, r1);
        assert_eq!(group.len(), 2);
        let mut values = HashMap::from([(a, "0".to_string()), (b, "1".to_string()), (n, "0".to_string())]);
        values.insert(group[0], "0".into());
        values.insert(group[1], "1".into());
        let req = submit(&dom, &base(), go, &values).unwrap().unwrap();
        assert_eq!(req.url().query(), Some("b=on&r=2&n=1&go="));
    }

    #[test]
    fn post_and_refusals() {
        let dom = html::parse("<form method=POST action=https://example.org/s><input name=q value=x></form>");
        let q = element_by_name(&dom, "q").unwrap().id();
        let req = submit(&dom, &base(), q, &HashMap::new()).unwrap().unwrap();
        assert_eq!(req, Request::Post(Url::parse("https://example.org/s").unwrap(), vec![("q".into(), "x".into())]));

        let dom = html::parse("<form><input name=user><input type=password name=pw></form>");
        let user = element_by_name(&dom, "user").unwrap().id();
        assert!(submit(&dom, &base(), user, &HashMap::new()).unwrap().is_err());

        let dom = html::parse("<input name=lonely>");
        let lonely = element_by_name(&dom, "lonely").unwrap().id();
        assert!(submit(&dom, &base(), lonely, &HashMap::new()).is_none(), "no enclosing form");
    }
}
