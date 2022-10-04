// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An implementation of Matrix push rules.
//!
//! The `Cow<_>` type is used extensively within this module to allow creating
//! the base rules as constants (in Rust constants can't require explicit
//! allocation atm).
//!
//! ---
//!
//! Push rules is the system used to determine which events trigger a push (and a
//! bump in notification counts).
//!
//! This consists of a list of "push rules" for each user, where a push rule is a
//! pair of "conditions" and "actions". When a user receives an event Synapse
//! iterates over the list of push rules until it finds one where all the conditions
//! match the event, at which point "actions" describe the outcome (e.g. notify,
//! highlight, etc).
//!
//! Push rules are split up into 5 different "kinds" (aka "priority classes"), which
//! are run in order:
//!     1. Override — highest priority rules, e.g. always ignore notices
//!     2. Content — content specific rules, e.g. @ notifications
//!     3. Room — per room rules, e.g. enable/disable notifications for all messages
//!        in a room
//!     4. Sender — per sender rules, e.g. never notify for messages from a given
//!        user
//!     5. Underride — the lowest priority "default" rules, e.g. notify for every
//!        message.
//!
//! The set of "base rules" are the list of rules that every user has by default. A
//! user can modify their copy of the push rules in one of three ways:
//!     1. Adding a new push rule of a certain kind
//!     2. Changing the actions of a base rule
//!     3. Enabling/disabling a base rule.
//!
//! The base rules are split into whether they come before or after a particular
//! kind, so the order of push rule evaluation would be: base rules for before
//! "override" kind, user defined "override" rules, base rules after "override"
//! kind, etc, etc.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Error};
use log::warn;
use pyo3::prelude::*;
use pythonize::{depythonize, pythonize};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use self::evaluator::PushRuleEvaluator;

mod base_rules;
pub mod evaluator;
pub mod utils;

/// Called when registering modules with python.
pub fn register_module(py: Python<'_>, m: &PyModule) -> PyResult<()> {
    let child_module = PyModule::new(py, "push")?;
    child_module.add_class::<PushRule>()?;
    child_module.add_class::<PushRules>()?;
    child_module.add_class::<FilteredPushRules>()?;
    child_module.add_class::<PushRuleEvaluator>()?;
    child_module.add_function(wrap_pyfunction!(get_base_rule_ids, m)?)?;

    m.add_submodule(child_module)?;

    // We need to manually add the module to sys.modules to make `from
    // synapse.synapse_rust import push` work.
    py.import("sys")?
        .getattr("modules")?
        .set_item("synapse.synapse_rust.push", child_module)?;

    Ok(())
}

#[pyfunction]
fn get_base_rule_ids() -> HashSet<&'static str> {
    base_rules::BASE_RULES_BY_ID.keys().copied().collect()
}

/// A single push rule for a user.
#[derive(Debug, Clone)]
#[pyclass(frozen)]
pub struct PushRule {
    /// A unique ID for this rule
    pub rule_id: Cow<'static, str>,
    /// The "kind" of push rule this is (see `PRIORITY_CLASS_MAP` in Python)
    #[pyo3(get)]
    pub priority_class: i32,
    /// The conditions that must all match for actions to be applied
    pub conditions: Cow<'static, [Condition]>,
    /// The actions to apply if all conditions are met
    pub actions: Cow<'static, [Action]>,
    /// Whether this is a base rule
    #[pyo3(get)]
    pub default: bool,
    /// Whether this is enabled by default
    #[pyo3(get)]
    pub default_enabled: bool,
}

#[pymethods]
impl PushRule {
    #[staticmethod]
    pub fn from_db(
        rule_id: String,
        priority_class: i32,
        conditions: &str,
        actions: &str,
    ) -> Result<PushRule, Error> {
        let conditions = serde_json::from_str(conditions).context("parsing conditions")?;
        let actions = serde_json::from_str(actions).context("parsing actions")?;

        Ok(PushRule {
            rule_id: Cow::Owned(rule_id),
            priority_class,
            conditions,
            actions,
            default: false,
            default_enabled: true,
        })
    }

    #[getter]
    fn rule_id(&self) -> &str {
        &self.rule_id
    }

    #[getter]
    fn actions(&self) -> Vec<Action> {
        self.actions.clone().into_owned()
    }

    #[getter]
    fn conditions(&self) -> Vec<Condition> {
        self.conditions.clone().into_owned()
    }

    fn __repr__(&self) -> String {
        format!(
            "<PushRule rule_id={}, conditions={:?}, actions={:?}>",
            self.rule_id, self.conditions, self.actions
        )
    }
}

/// The "action" Synapse should perform for a matching push rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    DontNotify,
    Notify,
    Coalesce,
    SetTweak(SetTweak),

    // An unrecognized custom action.
    Unknown(Value),
}

impl IntoPy<PyObject> for Action {
    fn into_py(self, py: Python<'_>) -> PyObject {
        // When we pass the `Action` struct to Python we want it to be converted
        // to a dict. We use `pythonize`, which converts the struct using the
        // `serde` serialization.
        pythonize(py, &self).expect("valid action")
    }
}

/// The body of a `SetTweak` push action.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SetTweak {
    set_tweak: Cow<'static, str>,

    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<TweakValue>,

    // This picks up any other fields that may have been added by clients.
    // These get added when we convert the `Action` to a python object.
    #[serde(flatten)]
    other_keys: Value,
}

/// The value of a `set_tweak`.
///
/// We need this (rather than using `TweakValue` directly) so that we can use
/// `&'static str` in the value when defining the constant base rules.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum TweakValue {
    String(Cow<'static, str>),
    Other(Value),
}

impl Serialize for Action {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Action::DontNotify => serializer.serialize_str("dont_notify"),
            Action::Notify => serializer.serialize_str("notify"),
            Action::Coalesce => serializer.serialize_str("coalesce"),
            Action::SetTweak(tweak) => tweak.serialize(serializer),
            Action::Unknown(value) => value.serialize(serializer),
        }
    }
}

/// Simple helper class for deserializing Action from JSON.
#[derive(Deserialize)]
#[serde(untagged)]
enum ActionDeserializeHelper {
    Str(String),
    SetTweak(SetTweak),
    Unknown(Value),
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let helper: ActionDeserializeHelper = Deserialize::deserialize(deserializer)?;
        match helper {
            ActionDeserializeHelper::Str(s) => match &*s {
                "dont_notify" => Ok(Action::DontNotify),
                "notify" => Ok(Action::Notify),
                "coalesce" => Ok(Action::Coalesce),
                _ => Err(D::Error::custom("unrecognized action")),
            },
            ActionDeserializeHelper::SetTweak(set_tweak) => Ok(Action::SetTweak(set_tweak)),
            ActionDeserializeHelper::Unknown(value) => Ok(Action::Unknown(value)),
        }
    }
}

/// A condition used in push rules to match against an event.
///
/// We need this split as `serde` doesn't give us the ability to have a
/// "catchall" variant in tagged enums.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Condition {
    /// A recognized condition that we can match against
    Known(KnownCondition),
    /// An unrecognized condition that we ignore.
    Unknown(Value),
}

/// The set of "known" conditions that we can handle.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
#[serde(tag = "kind")]
pub enum KnownCondition {
    EventMatch(EventMatchCondition),
    ContainsDisplayName,
    RoomMemberCount {
        #[serde(skip_serializing_if = "Option::is_none")]
        is: Option<Cow<'static, str>>,
    },
    SenderNotificationPermission {
        key: Cow<'static, str>,
    },
    #[serde(rename = "org.matrix.msc3772.relation_match")]
    RelationMatch {
        rel_type: Cow<'static, str>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
        event_type_pattern: Option<Cow<'static, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender: Option<Cow<'static, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender_type: Option<Cow<'static, str>>,
    },
}

impl IntoPy<PyObject> for Condition {
    fn into_py(self, py: Python<'_>) -> PyObject {
        pythonize(py, &self).expect("valid condition")
    }
}

impl<'source> FromPyObject<'source> for Condition {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        Ok(depythonize(ob)?)
    }
}

/// The body of a [`Condition::EventMatch`]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventMatchCondition {
    pub key: Cow<'static, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<Cow<'static, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern_type: Option<Cow<'static, str>>,
}

/// The collection of push rules for a user.
#[derive(Debug, Clone, Default)]
#[pyclass(frozen)]
pub struct PushRules {
    /// Custom push rules that override a base rule.
    overridden_base_rules: HashMap<Cow<'static, str>, PushRule>,

    /// Custom rules that come between the prepend/append override base rules.
    override_rules: Vec<PushRule>,
    /// Custom rules that come before the base content rules.
    content: Vec<PushRule>,
    /// Custom rules that come before the base room rules.
    room: Vec<PushRule>,
    /// Custom rules that come before the base sender rules.
    sender: Vec<PushRule>,
    /// Custom rules that come before the base underride rules.
    underride: Vec<PushRule>,
}

#[pymethods]
impl PushRules {
    #[new]
    pub fn new(rules: Vec<PushRule>) -> PushRules {
        let mut push_rules: PushRules = Default::default();

        for rule in rules {
            if let Some(&o) = base_rules::BASE_RULES_BY_ID.get(&*rule.rule_id) {
                push_rules.overridden_base_rules.insert(
                    rule.rule_id.clone(),
                    PushRule {
                        actions: rule.actions.clone(),
                        ..o.clone()
                    },
                );

                continue;
            }

            match rule.priority_class {
                5 => push_rules.override_rules.push(rule),
                4 => push_rules.content.push(rule),
                3 => push_rules.room.push(rule),
                2 => push_rules.sender.push(rule),
                1 => push_rules.underride.push(rule),
                _ => {
                    warn!(
                        "Unrecognized priority class for rule {}: {}",
                        rule.rule_id, rule.priority_class
                    );
                }
            }
        }

        push_rules
    }

    /// Returns the list of all rules, including base rules, in the order they
    /// should be executed in.
    fn rules(&self) -> Vec<PushRule> {
        self.iter().cloned().collect()
    }
}

impl PushRules {
    /// Iterates over all the rules, including base rules, in the order they
    /// should be executed in.
    pub fn iter(&self) -> impl Iterator<Item = &PushRule> {
        base_rules::BASE_PREPEND_OVERRIDE_RULES
            .iter()
            .chain(self.override_rules.iter())
            .chain(base_rules::BASE_APPEND_OVERRIDE_RULES.iter())
            .chain(self.content.iter())
            .chain(base_rules::BASE_APPEND_CONTENT_RULES.iter())
            .chain(self.room.iter())
            .chain(self.sender.iter())
            .chain(self.underride.iter())
            .chain(base_rules::BASE_APPEND_UNDERRIDE_RULES.iter())
            .map(|rule| {
                self.overridden_base_rules
                    .get(&*rule.rule_id)
                    .unwrap_or(rule)
            })
    }
}

/// A wrapper around `PushRules` that checks the enabled state of rules and
/// filters out disabled experimental rules.
#[derive(Debug, Clone, Default)]
#[pyclass(frozen)]
pub struct FilteredPushRules {
    push_rules: PushRules,
    enabled_map: BTreeMap<String, bool>,
    msc3772_enabled: bool,
}

#[pymethods]
impl FilteredPushRules {
    #[new]
    pub fn py_new(
        push_rules: PushRules,
        enabled_map: BTreeMap<String, bool>,
        msc3772_enabled: bool,
    ) -> Self {
        Self {
            push_rules,
            enabled_map,
            msc3772_enabled,
        }
    }

    /// Returns the list of all rules and their enabled state, including base
    /// rules, in the order they should be executed in.
    fn rules(&self) -> Vec<(PushRule, bool)> {
        self.iter().map(|(r, e)| (r.clone(), e)).collect()
    }
}

impl FilteredPushRules {
    /// Iterates over all the rules and their enabled state, including base
    /// rules, in the order they should be executed in.
    fn iter(&self) -> impl Iterator<Item = (&PushRule, bool)> {
        self.push_rules
            .iter()
            .filter(|rule| {
                // Ignore disabled experimental push rules
                if !self.msc3772_enabled
                    && rule.rule_id == "global/underride/.org.matrix.msc3772.thread_reply"
                {
                    return false;
                }

                true
            })
            .map(|r| {
                let enabled = *self
                    .enabled_map
                    .get(&*r.rule_id)
                    .unwrap_or(&r.default_enabled);
                (r, enabled)
            })
    }
}

#[test]
fn test_serialize_condition() {
    let condition = Condition::Known(KnownCondition::EventMatch(EventMatchCondition {
        key: "content.body".into(),
        pattern: Some("coffee".into()),
        pattern_type: None,
    }));

    let json = serde_json::to_string(&condition).unwrap();
    assert_eq!(
        json,
        r#"{"kind":"event_match","key":"content.body","pattern":"coffee"}"#
    )
}

#[test]
fn test_deserialize_condition() {
    let json = r#"{"kind":"event_match","key":"content.body","pattern":"coffee"}"#;

    let _: Condition = serde_json::from_str(json).unwrap();
}

#[test]
fn test_deserialize_custom_condition() {
    let json = r#"{"kind":"custom_tag"}"#;

    let condition: Condition = serde_json::from_str(json).unwrap();
    assert!(matches!(condition, Condition::Unknown(_)));

    let new_json = serde_json::to_string(&condition).unwrap();
    assert_eq!(json, new_json);
}

#[test]
fn test_deserialize_action() {
    let _: Action = serde_json::from_str(r#""notify""#).unwrap();
    let _: Action = serde_json::from_str(r#""dont_notify""#).unwrap();
    let _: Action = serde_json::from_str(r#""coalesce""#).unwrap();
    let _: Action = serde_json::from_str(r#"{"set_tweak": "highlight"}"#).unwrap();
}

#[test]
fn test_custom_action() {
    let json = r#"{"some_custom":"action_fields"}"#;

    let action: Action = serde_json::from_str(json).unwrap();
    assert!(matches!(action, Action::Unknown(_)));

    let new_json = serde_json::to_string(&action).unwrap();
    assert_eq!(json, new_json);
}