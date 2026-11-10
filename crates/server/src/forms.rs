//! XEP-0004 data forms: reading submitted values and building forms, for
//! PEP node configuration, MUC room configuration and registration.

use std::collections::BTreeMap;

use tokio_xmpp::minidom::Element;

use crate::stanza;

pub const NS_DATA: &str = "jabber:x:data";

/// Field values of a submitted form, by `var` (FORM_TYPE included).
pub fn values(x: &Element) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in x.children().filter(|f| f.is("field", NS_DATA)) {
        let Some(var) = f.attr("var") else { continue };
        let vals: Vec<String> = f
            .children()
            .filter(|v| v.is("value", NS_DATA))
            .map(|v| v.text())
            .collect();
        out.entry(var.to_owned()).or_default().extend(vals);
    }
    out
}

/// The single value of `var`, if present.
pub fn single<'a>(v: &'a BTreeMap<String, Vec<String>>, var: &str) -> Option<&'a str> {
    v.get(var).and_then(|vals| vals.first()).map(String::as_str)
}

/// XEP-0004 boolean: `1`/`true` or `0`/`false`.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

/// Builds a form of type `form` or `result`.
pub struct FormBuilder {
    x: Element,
}

impl FormBuilder {
    pub fn new(ty: &str, form_type: &str) -> Self {
        let mut b = Self {
            x: stanza::element("x", NS_DATA, &[("type", ty)]),
        };
        b.field("FORM_TYPE", "hidden", None, &[form_type]);
        b
    }

    pub fn title(mut self, t: &str) -> Self {
        let mut e = Element::bare("title", NS_DATA);
        e.append_text(t);
        self.x.append_child(e);
        self
    }

    pub fn instructions(mut self, t: &str) -> Self {
        let mut e = Element::bare("instructions", NS_DATA);
        e.append_text(t);
        self.x.append_child(e);
        self
    }

    /// Add a field with a type, an optional label and values.
    pub fn field(&mut self, var: &str, ty: &str, label: Option<&str>, values: &[&str]) -> &mut Element {
        let mut f = stanza::element("field", NS_DATA, &[("var", var), ("type", ty)]);
        if let Some(l) = label {
            stanza::set_attr(&mut f, "label", l);
        }
        for v in values {
            let mut e = Element::bare("value", NS_DATA);
            e.append_text(*v);
            f.append_child(e);
        }
        self.x.append_child(f);
        self.x.children_mut().last().unwrap()
    }

    /// A `list-single` field with its options.
    pub fn list(&mut self, var: &str, label: Option<&str>, value: &str, options: &[&str]) {
        let f = self.field(var, "list-single", label, &[value]);
        for o in options {
            let mut opt = stanza::element("option", NS_DATA, &[]);
            let mut v = Element::bare("value", NS_DATA);
            v.append_text(*o);
            opt.append_child(v);
            f.append_child(opt);
        }
    }

    pub fn boolean(&mut self, var: &str, label: Option<&str>, value: bool) {
        self.field(var, "boolean", label, &[if value { "1" } else { "0" }]);
    }

    pub fn build(self) -> Element {
        self.x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_builds() {
        let mut b = FormBuilder::new("form", "urn:x");
        b.boolean("b", Some("B"), true);
        b.list("l", None, "two", &["one", "two"]);
        b.field("m", "list-multi", None, &["x", "y"]);
        let x = b.build();
        let v = values(&x);
        assert_eq!(single(&v, "FORM_TYPE"), Some("urn:x"));
        assert_eq!(single(&v, "b").and_then(parse_bool), Some(true));
        assert_eq!(single(&v, "l"), Some("two"));
        assert_eq!(v["m"], ["x", "y"]);
    }
}
