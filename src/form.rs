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
                            if let (Some(name), Some(_)) = (name, c.attr("checked")) {
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
                    if let (Some(name), Some(option)) = (name, c.selected_option()) {
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
