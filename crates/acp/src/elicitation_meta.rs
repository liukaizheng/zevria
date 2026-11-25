//! Zevria's typed metadata carried by ACP elicitation properties and choices.

use serde::Serialize;
use serde_json::{Map, Value};

pub struct ZevriaElicitationMeta;

#[derive(Serialize)]
pub struct OptionDetail<'a> {
    pub description: Option<&'a str>,
    pub detail: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomAnswer<'a> {
    pub question_id: &'a str,
    pub other_value: &'a str,
}

impl ZevriaElicitationMeta {
    pub fn option(detail: OptionDetail<'_>) -> Map<String, Value> {
        Map::from_iter([("zevria".into(), serde_json::json!(detail))])
    }

    pub fn custom_answer(answer: CustomAnswer<'_>) -> Map<String, Value> {
        let mut value = serde_json::json!(answer);
        value["isOtherAnswer"] = Value::Bool(true);
        Map::from_iter([("zevria".into(), value)])
    }

    pub fn read_option(meta: Option<&Map<String, Value>>) -> Option<OptionDetail<'_>> {
        let value = meta?.get("zevria")?;
        Some(OptionDetail {
            description: value.get("description").and_then(Value::as_str),
            detail: value.get("detail")?.as_str()?,
        })
    }

    pub fn read_custom_answer(
        meta: &Map<String, Value>,
    ) -> Result<Option<CustomAnswer<'_>>, &'static str> {
        let Some(value) = meta.get("zevria") else {
            return Ok(None);
        };
        let object = value
            .as_object()
            .ok_or("custom-answer metadata must be an object")?;
        let Some(flag) = object.get("isOtherAnswer") else {
            return Ok(None);
        };
        if !flag
            .as_bool()
            .ok_or("custom-answer metadata flag must be boolean")?
        {
            return Ok(None);
        }
        let question_id = object
            .get("questionId")
            .and_then(Value::as_str)
            .ok_or("custom-answer metadata is missing questionId")?;
        if question_id.trim().is_empty() {
            return Err("custom-answer metadata questionId must not be blank");
        }
        let other_value = object
            .get("otherValue")
            .and_then(Value::as_str)
            .ok_or("native custom-answer metadata is missing otherValue")?;
        if other_value.trim().is_empty() {
            return Err("native custom-answer metadata otherValue must not be blank");
        }
        Ok(Some(CustomAnswer {
            question_id,
            other_value,
        }))
    }
}
