/*
 * Copyright (C) 2022 Vaticle
 *
 * Licensed to the Apache Software Foundation (ASF) under one
 * or more contributor license agreements.  See the NOTICE file
 * distributed with this work for additional information
 * regarding copyright ownership.  The ASF licenses this file
 * to you under the Apache License, Version 2.0 (the
 * "License"); you may not use this file except in compliance
 * with the License.  You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing,
 * software distributed under the License is distributed on an
 * "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 * KIND, either express or implied.  See the License for the
 * specific language governing permissions and limitations
 * under the License.
 */

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use cucumber::gherkin::Step;
use futures::{TryFutureExt, TryStreamExt};
use regex::{Captures, Regex};
use std::collections::HashMap;
use typedb_client::{
    answer::ConceptMap,
    concept::{
        Attribute, AttributeType, Concept,
        Entity, EntityType,
        Relation, RelationType, RoleType, RootThingType, ScopedLabel,
        Thing, ThingType, Value, ValueType,
    },
};
use typeql_lang::{
    parse_pattern, parse_queries, parse_query,
    pattern::{ThingVariableBuilder, TypeVariableBuilder},
    query::{AggregateQueryBuilder, TypeQLDefine, TypeQLInsert, TypeQLMatch, TypeQLUndefine},
    typeql_match, var,
};

use crate::behaviour::Context;

pub fn iter_table(step: &Step) -> impl Iterator<Item = &str> {
    step.table().unwrap().rows.iter().flatten().map(String::as_str)
}

pub fn iter_map_table(step: &Step) -> impl Iterator<Item = HashMap<&String, &String>> {
    let (keys, rows) = step.table().unwrap().rows.split_first().unwrap();
    rows.iter().map(|row| keys.iter().zip(row).collect())
}

fn format_double(double: &f64) -> String {
    let formatted = format!("{:.12}", double).trim_end_matches('0').to_string();
    if formatted.ends_with('.') {
        formatted + "0"
    } else {
        formatted
    }
}

fn format_datetime(datetime: &NaiveDateTime) -> String {
    if datetime.time() == NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
        format!("{0}", datetime.date())
    } else {
        format!("{0}", datetime)
    }
}

fn iid_to_string(iid: &Vec<u8>) -> String {
    let mut s = String::from("0x");
    for byte in iid {
        s += format!("{:02x}", byte).as_str();
    }
    s
}

fn get_iid(concept: &Concept) -> String {
    let iid = match concept {
        Concept::Entity(Entity { iid, ..}) => iid,
        Concept::Attribute(Attribute { iid, ..}) => iid,
        Concept::Relation(Relation { iid, ..}) => iid,
        _ => unreachable!(),
    };
    format!("0x{iid}")
}

async fn get_attribute_concept(context: &Context, iid: String, attr_label: &str) -> Result<Concept, String> {
    let query = typeql_match!(var("x").iid(iid).has((attr_label, var("val"))),).get(["val"]).to_string();

    let stream = context.transaction().query().match_(&query);
    match stream {
        Ok(_) => {
            let res = stream.unwrap().try_collect::<Vec<_>>().await;
            match res {
                Ok(_) => Ok(res.unwrap()[0].map["val"].clone()),
                Err(error) => Err(error.to_string()),
            }
        }
        Err(error) => Err(error.to_string()),
    }

    // context.transaction().query().match_(&query)
    //     .map_err(|error| error.to_string()).unwrap().try_collect::<Vec<_>>().await
    //     .map_err(|error| error.to_string()).map(|res| res[0].map["val"].clone())
}

pub fn equals_approximate(first: f64, second: f64) -> bool {
    const EPS: f64 = 1e-4;
    return (first - second).abs() < EPS;
}

fn values_equal(identifiers: &str, answer: &Concept) -> bool {
    let attribute: Vec<&str> = identifiers.splitn(2, ":").collect();
    assert_eq!(attribute.len(), 2, "Unexpected table cell format: {identifiers}.");
    match answer {
        Concept::Attribute(Attribute { type_: AttributeType { label, value_type, .. }, value, .. } ) => match value {
            Value::String(val) => {
                label == attribute[0] && val == attribute[1]
            },
            Value::Long(val) => {
                label == attribute[0]
                    && attribute[1]
                        .parse::<i64>()
                        .and_then(|expected| Ok(expected.eq(val)))
                        .unwrap_or_else(|_| false)
            },
            Value::Double(val) => {
                label == attribute[0]
                    && attribute[1]
                        .parse::<f64>()
                        .and_then(|expected| Ok(equals_approximate(expected, *val)))
                        .unwrap_or_else(|_| false)
            },
            Value::Boolean(val) => {
                label == attribute[0]
                    && attribute[1]
                        .parse::<bool>()
                        .and_then(|expected| Ok(expected.eq(val)))
                        .unwrap_or_else(|_| false)
            },
            Value::DateTime(val) => {
                label == attribute[0] && format_datetime(val) == attribute[1]
            },
        },
        _ => false,
    }
}

fn labels_equal(identifier: &str, answer: &Concept) -> bool {
    let mut binding = String::new();
    let label = match answer {
        Concept::EntityType(type_) => { binding = type_.clone().label; &binding },
        Concept::RoleType(RoleType { label: ScopedLabel { scope, name }, ..}) => {
            binding = format!("{scope}:{name}");
            &binding
        },
        Concept::Entity(Entity { type_: EntityType { label, .. }, .. }) => label,
        Concept::Relation(Relation { type_: RelationType { label, .. }, .. }) => label,
        Concept::RootThingType(_) => { binding = String::from("thing"); &binding },
        Concept::RelationType(RelationType { label, .. }) => label,
        Concept::AttributeType(AttributeType { label, .. }) => label,
        Concept::Attribute(Attribute { type_: AttributeType { label, .. }, .. }) => label,
        _ => unreachable!(),
    };
    label == identifier
}

async fn key_values_equal(context: &Context, identifiers: &str, answer: &Concept) -> bool {
    let attribute: Vec<&str> = identifiers.splitn(2, ":").collect();
    assert_eq!(attribute.len(), 2, "Unexpected table cell format: {identifiers}.");
    let attribute_concept = get_attribute_concept(context, get_iid(answer), attribute[0]).await;
    match attribute_concept {
        Ok(_) => values_equal(identifiers, &attribute_concept.unwrap()),
        _ => false,
    }
}

pub async fn match_answer_concept(context: &Context, answer_identifier: &String, answer: &Concept) -> bool {
    let identifiers: Vec<&str> = answer_identifier.splitn(2, ":").collect();
    match identifiers[0] {
        "key" => key_values_equal(context, identifiers[1], answer).await,
        "label" => labels_equal(identifiers[1], answer),
        "value" => values_equal(identifiers[1], answer),
        _ => unreachable!(),
    }
}

pub async fn match_answer_concept_map(
    context: &Context,
    answer_identifiers: &HashMap<&String, &String>,
    answer: &ConceptMap,
) -> bool {
    for key in answer_identifiers.keys() {
        if !(answer.map.contains_key(key.clone())
            && match_answer_concept(context, answer_identifiers.get(key).unwrap(), answer.get(key).unwrap()).await)
        {
            return false;
        }
    }
    true
}

pub fn apply_query_template(query_template: &String, answer: &ConceptMap) -> String {
    let re = Regex::new(r"<answer\.(.+?)\.iid>").unwrap();
    re.replace_all(query_template, |caps: &Captures| format!("{}", get_iid(&answer.map.get(&caps[1]).unwrap())))
        .to_string()
}
