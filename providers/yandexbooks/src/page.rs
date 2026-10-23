//! The data Yandex Books renders into its pages, so the provider never calls the site's API, which
//! robots.txt disallows. Each page carries the site's own GraphQL answers as
//! `{"type":"next","value":{"data":{...}}}` events: search results in an Apollo transport script as
//! plain JSON, a book's details inside the Next.js flight string, escaped.

use serde_json::Value;

const EVENT: &str = r#""type":"next","value":"#;
const FLIGHT: &str = "self.__next_f.push([1,";

/// Every `data` entry of every answer on the page, in page order.
pub fn answers(html: &str) -> Vec<(String, Value)> {
    let mut found = Vec::new();
    collect(html, &mut found);
    for (at, _) in html.match_indices(FLIGHT) {
        let rest = &html[at + FLIGHT.len()..];
        if let Some(Ok(flight)) = serde_json::Deserializer::from_str(rest)
            .into_iter::<String>()
            .next()
        {
            collect(&flight, &mut found);
        }
    }
    found
}

fn collect(text: &str, found: &mut Vec<(String, Value)>) {
    for (at, _) in text.match_indices(EVENT) {
        let rest = &text[at + EVENT.len()..];
        let Some(Ok(value)) = serde_json::Deserializer::from_str(rest)
            .into_iter::<Value>()
            .next()
        else {
            continue;
        };
        if let Some(Value::Object(data)) = value.get("data") {
            found.extend(data.iter().map(|(key, value)| (key.clone(), value.clone())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(html: &str) -> Vec<String> {
        answers(html).into_iter().map(|(key, _)| key).collect()
    }

    #[test]
    fn reads_the_apollo_script_and_the_flight_string() {
        assert_eq!(
            keys(include_str!("../testdata/search-master-i-margarita.html")),
            ["search"]
        );
        assert_eq!(
            keys(include_str!("../testdata/book-MWxAop5b.html")),
            ["textBook", "oldBook"]
        );
    }

    #[test]
    fn skips_what_does_not_parse() {
        let html = r#"<script>x({"type":"next","value":{"data":{"a":1</script>
            <script>self.__next_f.push([1,"0:{\"type\":\"next\",\"value\":{\"data\":{\"b\":2}}}"])</script>
            <script>self.__next_f.push([1,"unterminated])</script>"#;
        assert_eq!(keys(html), ["b"]);
        assert!(answers("<html></html>").is_empty());
    }
}
