//! Read hook structure while retaining the exact source spans of editable strings.
use anyhow::{Result, anyhow, bail};
use saphyr_parser::{Event, Parser, Span};
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use toml_edit::Item;

#[derive(Debug, Clone)]
pub(super) struct Scalar {
    pub value: String,
    pub span: Option<Range<usize>>,
}

impl Scalar {
    fn new(value: &str, span: Option<Range<usize>>, content: &str) -> Self {
        // Only edit a literal spelling of the decoded value. Escapes, folded
        // strings and shared YAML anchors cannot be rewritten unambiguously.
        let span = span.and_then(|span| {
            let raw = content.get(span.clone())?;
            if raw == value {
                Some(span)
            } else if raw.len() >= 2
                && matches!(raw.as_bytes()[0], b'\'' | b'"')
                && raw.as_bytes().first() == raw.as_bytes().last()
                && raw.get(1..raw.len() - 1) == Some(value)
            {
                Some(span.start + 1..span.end - 1)
            } else {
                None
            }
        });
        Self {
            value: value.into(),
            span,
        }
    }

    pub fn line(&self, content: &str) -> Option<usize> {
        self.span
            .as_ref()
            .map(|s| content[..s.start].bytes().filter(|b| *b == b'\n').count() + 1)
    }
}

#[derive(Debug)]
pub(super) enum Node {
    Scalar(Scalar),
    Sequence(Vec<Node>),
    Mapping(HashMap<String, Node>),
    Unsupported,
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Self::Mapping(m) => m.get(key),
            _ => None,
        }
    }
    pub fn scalar(&self) -> Option<&Scalar> {
        match self {
            Self::Scalar(s) => Some(s),
            _ => None,
        }
    }
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key)?.scalar().map(|s| s.value.as_str())
    }
    pub fn sequence(&self) -> &[Node] {
        match self {
            Self::Sequence(s) => s,
            _ => &[],
        }
    }
}

pub(super) fn yaml(content: &str) -> Result<Node> {
    let offsets: Vec<_> = content
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(content.len()))
        .collect();
    let events = Parser::new_from_str(content).collect::<Result<Vec<_>, _>>()?;
    let mut events: VecDeque<_> = events.into();
    let mut document = None;
    while let Some((event, _)) = events.front() {
        if matches!(
            event,
            Event::StreamStart | Event::DocumentStart(_) | Event::DocumentEnd | Event::StreamEnd
        ) {
            events.pop_front();
            continue;
        }
        if document.is_some() {
            bail!("Expected one YAML document");
        }
        document = Some(yaml_node(&mut events, content, &offsets, 0)?);
    }
    Ok(document.unwrap_or(Node::Unsupported))
}

fn yaml_node<'a>(
    events: &mut VecDeque<(Event<'a>, Span)>,
    content: &str,
    offsets: &[usize],
    depth: usize,
) -> Result<Node> {
    if depth > 128 {
        bail!("Hook configuration is nested too deeply");
    }
    let (event, span) = events
        .pop_front()
        .ok_or_else(|| anyhow!("Incomplete YAML document"))?;
    match event {
        Event::Scalar(value, _, anchor, tag) => Ok(Node::Scalar(Scalar::new(
            &value,
            (anchor == 0 && tag.is_none())
                .then_some(offsets[span.start.index()]..offsets[span.end.index()]),
            content,
        ))),
        event @ (Event::SequenceStart(..) | Event::MappingStart(..)) => {
            let mapping = matches!(event, Event::MappingStart(..));
            let (Event::SequenceStart(anchor, tag) | Event::MappingStart(anchor, tag)) = event
            else {
                unreachable!()
            };
            let mut nodes = Vec::new();
            loop {
                let next = events
                    .pop_front()
                    .ok_or_else(|| anyhow!("Incomplete YAML collection"))?;
                if matches!(next.0, Event::SequenceEnd | Event::MappingEnd) {
                    break;
                }
                events.push_front(next);
                nodes.push(yaml_node(events, content, offsets, depth + 1)?);
            }
            if anchor != 0 || tag.is_some() {
                return Ok(Node::Unsupported);
            }
            if !mapping {
                return Ok(Node::Sequence(nodes));
            }
            let mut map = HashMap::new();
            let mut nodes = nodes.into_iter();
            while let Some(key) = nodes.next() {
                let key = key
                    .scalar()
                    .ok_or_else(|| anyhow!("Expected a string YAML key"))?
                    .value
                    .clone();
                let value = nodes
                    .next()
                    .ok_or_else(|| anyhow!("Missing YAML mapping value"))?;
                if key == "<<" {
                    return Ok(Node::Unsupported);
                }
                if map.insert(key.clone(), value).is_some() {
                    bail!("Duplicate YAML key '{key}'");
                }
            }
            Ok(Node::Mapping(map))
        }
        Event::Alias(_) => Ok(Node::Unsupported),
        _ => bail!("Unexpected YAML event"),
    }
}

pub(super) fn toml(content: &str) -> Result<Node> {
    let document = content.parse::<toml_edit::Document<String>>()?;
    Ok(toml_node(document.as_item(), content))
}

fn toml_node(item: &Item, content: &str) -> Node {
    if let Some(value) = item.as_str() {
        Node::Scalar(Scalar::new(value, item.span(), content))
    } else if let Some(table) = item.as_table_like() {
        Node::Mapping(
            table
                .iter()
                .map(|(k, v)| (k.into(), toml_node(v, content)))
                .collect(),
        )
    } else if let Some(array) = item.as_array_of_tables() {
        Node::Sequence(
            array
                .iter()
                .map(|table| {
                    Node::Mapping(
                        table
                            .iter()
                            .map(|(k, v)| (k.into(), toml_node(v, content)))
                            .collect(),
                    )
                })
                .collect(),
        )
    } else if let Some(array) = item.as_array() {
        Node::Sequence(
            array
                .iter()
                .map(|v| toml_node(&Item::Value(v.clone()), content))
                .collect(),
        )
    } else {
        Node::Unsupported
    }
}
