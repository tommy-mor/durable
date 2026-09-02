use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ranking::Comparison;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Init {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        id: String,
        #[serde(default)]
        capacity: i64,
        #[serde(default)]
        model: String,
        #[serde(default, rename = "vote_model")]
        vote_model: String,
        #[serde(default, rename = "api_key")]
        api_key: String,
    },
    Thought {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        content: String,
        #[serde(default)]
        id: String,
    },
    Perception {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        content: String,
        #[serde(default)]
        id: String,
    },
    Response {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        content: String,
        #[serde(default)]
        id: String,
    },
    Declaration {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        content: String,
        #[serde(default)]
        id: String,
    },
    Vote {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        vote_a_id: String,
        #[serde(default)]
        vote_b_id: String,
        #[serde(default)]
        vote_score: i32,
        #[serde(default)]
        reasoning: String,
    },
    Compaction {
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        kept_ids: Vec<String>,
        #[serde(default)]
        released_ids: Vec<String>,
    },
}

impl Event {
    pub fn id(&self) -> Option<&str> {
        match self {
            Event::Init { id, .. }
            | Event::Thought { id, .. }
            | Event::Perception { id, .. }
            | Event::Response { id, .. }
            | Event::Declaration { id, .. } => {
                if id.is_empty() {
                    None
                } else {
                    Some(id)
                }
            }
            _ => None,
        }
    }

    fn is_memory(&self) -> bool {
        matches!(
            self,
            Event::Thought { .. } | Event::Perception { .. } | Event::Response { .. }
        )
    }

    /// Drop secrets before anything lands in a Hop projection.
    pub fn sanitized(&self) -> Event {
        match self {
            Event::Init {
                timestamp,
                id,
                capacity,
                model,
                vote_model,
                ..
            } => Event::Init {
                timestamp: *timestamp,
                id: id.clone(),
                capacity: *capacity,
                model: model.clone(),
                vote_model: vote_model.clone(),
                api_key: String::new(),
            },
            other => other.clone(),
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).expect("event serializes");
        if let Some(obj) = v.as_object_mut() {
            obj.remove("api_key");
        }
        v
    }
}

#[derive(Debug, Clone, Default)]
pub struct Projection {
    pub all: BTreeMap<String, Event>,
    pub current: BTreeMap<String, Event>,
    pub votes: BTreeMap<String, Comparison>,
    pub declaration: String,
    pub capacity: i64,
    pub model: String,
    pub vote_model: String,
    pub memories: i64,
    pub events: Vec<Event>,
}

impl Projection {
    pub fn apply(&mut self, raw: Event) {
        let event = raw.sanitized();
        match &event {
            Event::Vote {
                vote_a_id,
                vote_b_id,
                vote_score,
                ..
            } => {
                if vote_a_id.is_empty() || vote_b_id.is_empty() {
                    self.events.push(event);
                    return;
                }
                let (low, high, score) = if vote_a_id < vote_b_id {
                    (vote_a_id.clone(), vote_b_id.clone(), *vote_score)
                } else {
                    (vote_b_id.clone(), vote_a_id.clone(), -vote_score)
                };
                let key = format!("{low}|{high}");
                self.votes.insert(
                    key,
                    Comparison {
                        a_id: low,
                        b_id: high,
                        score,
                    },
                );
            }
            Event::Compaction {
                kept_ids,
                released_ids,
                ..
            } => {
                for rid in kept_ids {
                    if let Some(mem) = self.all.get(rid) {
                        self.current.insert(rid.clone(), mem.clone());
                    }
                }
                for rid in released_ids {
                    self.current.remove(rid);
                }
                self.recount();
            }
            Event::Init {
                id,
                capacity,
                model,
                vote_model,
                ..
            } => {
                self.capacity = *capacity;
                self.model = model.clone();
                self.vote_model = vote_model.clone();
                if !id.is_empty() {
                    self.current.insert(id.clone(), event.clone());
                    self.all.insert(id.clone(), event.clone());
                }
            }
            Event::Thought { id, .. }
            | Event::Perception { id, .. }
            | Event::Response { id, .. }
            | Event::Declaration { id, .. } => {
                if !id.is_empty() {
                    self.current.insert(id.clone(), event.clone());
                    self.all.insert(id.clone(), event.clone());
                }
                if matches!(event, Event::Declaration { .. }) {
                    if let Event::Declaration { content, .. } = &event {
                        self.declaration = content.clone();
                    }
                }
                if event.is_memory() {
                    self.memories += 1;
                }
            }
        }
        self.events.push(event);
    }

    fn recount(&mut self) {
        self.memories = self.current.values().filter(|e| e.is_memory()).count() as i64;
    }

    pub fn to_json(&self) -> Value {
        let map_of = |m: &BTreeMap<String, Event>| {
            let mut obj = serde_json::Map::new();
            for (k, e) in m {
                obj.insert(k.clone(), e.to_json());
            }
            Value::Object(obj)
        };
        let mut votes = serde_json::Map::new();
        for (k, c) in &self.votes {
            votes.insert(
                k.clone(),
                json!({ "a": c.a_id, "b": c.b_id, "score": c.score }),
            );
        }
        json!({
            "all": map_of(&self.all),
            "current": map_of(&self.current),
            "votes": Value::Object(votes),
            "declaration": self.declaration,
            "capacity": self.capacity,
            "model": self.model,
            "vote_model": self.vote_model,
            "memories": self.memories,
            "count": self.events.len() as i64,
            "events": self.events.iter().map(Event::to_json).collect::<Vec<_>>(),
        })
    }
}

pub fn project_json(events: &[Value]) -> Value {
    let mut p = Projection::default();
    for v in events {
        match serde_json::from_value::<Event>(v.clone()) {
            Ok(e) => p.apply(e),
            Err(_) => continue,
        }
    }
    p.to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_api_key_and_counts_memories() {
        let events = vec![
            json!({"type":"init","id":"i","capacity":6,"model":"m","vote_model":"v","api_key":"SECRET"}),
            json!({"type":"declaration","id":"d","content":"be kind"}),
            json!({"type":"perception","id":"p1","content":"hi"}),
            json!({"type":"response","id":"r1","content":"hello"}),
        ];
        let p = project_json(&events);
        assert!(p["all"]["i"].get("api_key").is_none());
        assert_eq!(p["memories"], 2);
        assert_eq!(p["declaration"], "be kind");
        assert_eq!(p["capacity"], 6);
    }

    #[test]
    fn compaction_releases_and_can_restore() {
        let events = vec![
            json!({"type":"perception","id":"a","content":"a"}),
            json!({"type":"perception","id":"b","content":"b"}),
            json!({"type":"compaction","kept_ids":["a"],"released_ids":["b"]}),
        ];
        let p = project_json(&events);
        assert_eq!(p["memories"], 1);
        assert!(p["current"].get("a").is_some());
        assert!(p["current"].get("b").is_none());
        assert!(p["all"].get("b").is_some());
    }
}
