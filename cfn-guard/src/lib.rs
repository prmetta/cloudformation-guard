// © Amazon Web Services, Inc. or its affiliates. All Rights Reserved. This AWS Content is provided subject to the terms of the AWS Customer Agreement available at http://aws.amazon.com/agreement or other written agreement between Customer and either Amazon Web Services, Inc. or Amazon Web Services EMEA SARL or both.

use log::{self, debug, error, info, trace};
use serde_json::Value;

use regex::Regex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error::Error;
use std::fs;

mod guard_types;
mod parser;
pub mod util;

pub use crate::guard_types::{enums, structs};
use crate::util::fix_stringified_bools;

pub fn run(
    template_file: &str,
    rules_file: &str,
    strict_checks: bool,
) -> Result<(Vec<String>, usize), Box<dyn Error>> {
    debug!("Entered run");
    let template_contents = fs::read_to_string(template_file)?;
    let rules_file_contents = fs::read_to_string(rules_file)?;

    trace!(
        "Template file is '{}' and its contents are:\n'{}'",
        template_file,
        template_contents
    );
    trace!(
        "Rules file is '{}' and its contents are: {}",
        rules_file,
        rules_file_contents.to_string()
    );

    match run_check(&template_contents, &rules_file_contents, strict_checks) {
        Ok(res) => {
            debug!("Outcome was: '{:#?}'", &res.0);
            Ok(res)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn run_check(
    template_file_contents: &str,
    rules_file_contents: &str,
    strict_checks: bool,
) -> Result<(Vec<String>, usize), String> {
    info!("Loading CloudFormation Template and Rule Set");
    debug!("Entered run_check");

    trace!("Normalizing booleans in inputs");
    let cleaned_template_file_contents = fix_stringified_bools(template_file_contents);
    trace!(
        "Cleaned template contents are:\n'{}'",
        cleaned_template_file_contents
    );

    let cleaned_rules_file_contents = fix_stringified_bools(rules_file_contents);
    trace!(
        "Cleaned rules file contents are:\n'{}'",
        cleaned_rules_file_contents
    );

    debug!("Deserializing CloudFormation template");
    let cfn_template: HashMap<String, Value> =
        match serde_json::from_str(&cleaned_template_file_contents) {
            Ok(s) => s,
            Err(_) => match serde_yaml::from_str(&cleaned_template_file_contents) {
                Ok(y) => y,
                Err(e) => {
                    return Err(format!(
                        "Template file format was unreadable as json or yaml: {}",
                        e
                    ));
                }
            },
        };
    trace!("CFN Template is '{:#?}'", &cfn_template);

    let cfn_resources: HashMap<String, Value> = match cfn_template.get("Resources") {
        Some(r) => serde_json::from_value(r.clone()).unwrap(),
        None => {
            return Err(
                "Template file does not contain a [Resources] section to check".to_string(),
            );
        }
    };

    trace!("CFN resources are: {:?}", cfn_resources);

    info!("Parsing rule set");
    match parser::parse_rules(&cleaned_rules_file_contents, &cfn_resources) {
        Ok(pr) => {
            let mut outcome: Vec<String> = vec![];
            match check_resources(&cfn_resources, &pr, strict_checks) {
                Some(x) => {
                    outcome.extend(x);
                }
                None => (),
            }
            outcome.sort();

            let exit_code = match outcome.len() {
                0 => 0,
                _ => 2,
            };
            return Ok((outcome, exit_code));
        }
        Err(e) => Err(e),
    }
}

fn check_resources(
    cfn_resources: &HashMap<String, Value>,
    parsed_rule_set: &structs::ParsedRuleSet,
    strict_checks: bool,
) -> Option<Vec<String>> {
    info!("Checking resources");
    let mut result: Vec<String> = vec![];
    for c_rule in parsed_rule_set.rule_set.iter() {
        info!("Applying rule '{:#?}'", &c_rule);
        match c_rule {
            enums::RuleType::SimpleRule(r) => {
                trace!("Simple rule is {:#?}", r);
                if let Some(rule_result) =
                    apply_rule(&cfn_resources, r, &parsed_rule_set.variables, strict_checks)
                {
                    result.extend(rule_result);
                }
            }
            enums::RuleType::ConditionalRule(r) => {
                trace!("Conditional rule is {:#?}", r);
                for (name, cfn_resource) in cfn_resources {
                    trace!("Checking condition: {:?}", r.condition);

                    let mut cfn_resource_map: HashMap<String, Value> = HashMap::new();
                    cfn_resource_map.insert(name.clone(), cfn_resource.clone());
                    trace!("Temporary resource map is {:#?}", cfn_resource_map);

                    let condition_rule_set = structs::ParsedRuleSet {
                        variables: parsed_rule_set.variables.clone(),
                        rule_set: vec![enums::RuleType::CompoundRule(r.clone().condition)],
                    };
                    trace!(
                        "condition_rule_set is {{variables: {:#?}, rule_set: {:#?}}}",
                        util::filter_for_env_vars(&condition_rule_set.variables),
                        condition_rule_set.rule_set
                    );

                    // Use the existing rules logic to see if there's a hit on the Condition clause
                    match check_resources(&cfn_resource_map, &condition_rule_set, true) {
                        Some(_) => (), // A result from a condition check means that it *wasn't* met (by def)
                        None => {
                            trace!("Condition met for {}", r.condition.raw_rule);
                            let consequent_rule_set = structs::ParsedRuleSet {
                                variables: parsed_rule_set.variables.clone(),
                                rule_set: vec![enums::RuleType::CompoundRule(r.clone().consequent)],
                            };
                            let postscript = format!("when {}", r.condition.raw_rule);
                            match check_resources(
                                &cfn_resource_map,
                                &consequent_rule_set,
                                strict_checks,
                            ) {
                                Some(x) => {
                                    let temp_result = x.into_iter().map(|x| {
                                        if !x.contains("when") {
                                            format!("{} {}", x, postscript)
                                        } else {
                                            x
                                        }
                                    });
                                    result.extend(temp_result);
                                }
                                None => (),
                            };
                        }
                    };
                }
            }
            enums::RuleType::CompoundRule(r) => match r.compound_type {
                enums::CompoundType::OR => {
                    for (name, cfn_resource) in cfn_resources {
                        trace!("OR'ing [{}] against {:?}", name, r);
                        let mut pass_fail = HashSet::new();
                        let mut temp_results: Vec<String> = vec![];
                        let mut cfn_resource_map: HashMap<String, Value> = HashMap::new();
                        cfn_resource_map.insert(name.clone(), cfn_resource.clone());
                        for typed_rule in &r.rule_list {
                            match typed_rule {
                                enums::RuleType::SimpleRule(r) => {
                                    match apply_rule(
                                        &cfn_resource_map,
                                        r,
                                        &parsed_rule_set.variables,
                                        strict_checks,
                                    ) {
                                        Some(rule_result) => {
                                            pass_fail.insert("fail");
                                            temp_results.extend(rule_result);
                                        }
                                        None => {
                                            pass_fail.insert("pass");
                                        }
                                    }
                                }
                                enums::RuleType::CompoundRule(r) => {
                                    let rule_set = structs::ParsedRuleSet {
                                        variables: parsed_rule_set.variables.clone(),
                                        rule_set: vec![enums::RuleType::CompoundRule(r.clone())],
                                    };
                                    let postscript = format!("when {}", r.raw_rule);
                                    match check_resources(
                                        &cfn_resource_map,
                                        &rule_set,
                                        strict_checks,
                                    ) {
                                        Some(x) => {
                                            let temp_result = x.into_iter().map(|x| {
                                                if !x.contains("when") {
                                                    format!("{} {}", x, postscript)
                                                } else {
                                                    x
                                                }
                                            });
                                            result.extend(temp_result);
                                        }
                                        None => (),
                                    };
                                }
                                enums::RuleType::ConditionalRule(r) => {
                                    let rule_set = structs::ParsedRuleSet {
                                        variables: parsed_rule_set.variables.clone(),
                                        rule_set: vec![enums::RuleType::ConditionalRule(r.clone())],
                                    };
                                    let postscript = format!("when {}", r.condition.raw_rule);
                                    match check_resources(
                                        &cfn_resource_map,
                                        &rule_set,
                                        strict_checks,
                                    ) {
                                        Some(x) => {
                                            let temp_result = x.into_iter().map(|x| {
                                                if !x.contains("when") {
                                                    format!("{} {}", x, postscript)
                                                } else {
                                                    x
                                                }
                                            });
                                            result.extend(temp_result);
                                        }
                                        None => (),
                                    };
                                }
                            }
                        }
                        trace! {"temp_results are {:?}", &temp_results};
                        trace! {"pass_fail set is {:?}", &pass_fail};
                        if !pass_fail.contains("pass") {
                            result.extend(temp_results);
                        }
                    }
                }
                enums::CompoundType::AND => {
                    for typed_rule in &r.rule_list {
                        match typed_rule {
                            enums::RuleType::SimpleRule(r) => {
                                if let Some(rule_result) = apply_rule(
                                    &cfn_resources,
                                    r,
                                    &parsed_rule_set.variables,
                                    strict_checks,
                                ) {
                                    result.extend(rule_result);
                                }
                            }
                            enums::RuleType::CompoundRule(r) => {
                                let rule_set = structs::ParsedRuleSet {
                                    variables: parsed_rule_set.variables.clone(),
                                    rule_set: vec![enums::RuleType::CompoundRule(r.clone())],
                                };
                                let postscript = format!("when {}", r.raw_rule);
                                match check_resources(cfn_resources, &rule_set, strict_checks) {
                                    Some(x) => {
                                        let temp_result = x.into_iter().map(|x| {
                                            if !x.contains("when") {
                                                format!("{} {}", x, postscript)
                                            } else {
                                                x
                                            }
                                        });
                                        result.extend(temp_result);
                                    }
                                    None => (),
                                };
                            }
                            enums::RuleType::ConditionalRule(r) => {
                                let rule_set = structs::ParsedRuleSet {
                                    variables: parsed_rule_set.variables.clone(),
                                    rule_set: vec![enums::RuleType::ConditionalRule(r.clone())],
                                };
                                let postscript = format!("when {}", r.condition.raw_rule);
                                match check_resources(cfn_resources, &rule_set, strict_checks) {
                                    Some(x) => {
                                        let temp_result = x.into_iter().map(|x| {
                                            if !x.contains("when") {
                                                format!("{} {}", x, postscript)
                                            } else {
                                                x
                                            }
                                        });
                                        result.extend(temp_result);
                                    }
                                    None => (),
                                };
                            }
                        }
                    }
                }
            },
        }
    }
    if !result.is_empty() {
        Some(result)
    } else {
        None
    }
}

fn apply_rule(
    cfn_resources: &HashMap<String, Value>,
    rule: &structs::Rule,
    variables: &HashMap<String, String>,
    strict_checks: bool,
) -> Option<Vec<String>> {
    debug!("Applying rule '{:?}'", &rule);
    let mut rule_result: Vec<String> = vec![];
    for (name, cfn_resource) in cfn_resources {
        if cfn_resource["Type"] == rule.resource_type {
            info!(
                "Checking [{}] which is of type {}",
                &name, &cfn_resource["Type"]
            );
            let mut target_field: Vec<&str> = rule.field.split('.').collect();
            let (property_root, address) = match target_field.first() {
                Some(x) => {
                    if *x == "" {
                        // If the first address segment is a '.'
                        target_field.remove(0);
                        target_field.insert(0, "."); // Replace the empty first element with a "."
                        (cfn_resource, target_field) // Return the root of the Value for lookup
                    } else {
                        (&cfn_resource["Properties"], target_field) // Otherwise, treat it as a normal property lookup
                    }
                }
                None => {
                    error!("Invalid property address: {}", rule.field);
                    return None;
                }
            };
            match util::get_resource_prop_value(property_root, &address) {
                Err(e) => {
                    if strict_checks {
                        rule_result.push(match &rule.custom_msg {
                            Some(c) => format!("[{}] failed because {}", name, c),
                            None => format!(
                        "[{}] failed because it does not contain the required property of [{}]",
                        name, e
                    ),
                        })
                    }
                }
                Ok(val) => {
                    debug!("Template val is {:?}", val);
                    match util::deref_rule_value(rule, variables) {
                        Ok(v) => {
                            debug!("rule_val is {} and val is {}", &v, &val);
                            if let Some(r) = apply_rule_operation(name, rule, v, &val) {
                                rule_result.push(r)
                            }
                        }
                        Err(_) => rule_result.push(
                            format!("[{}] failed because there is no value defined for [{}] to check [{}] against",
                                    name,
                                    rule.value,
                                    rule.field)),
                    };
                }
            };
        } else {
            info!(
                "Rule does not apply to {} which is of type {}",
                &name, cfn_resource["Type"]
            );
        };
    }
    if !rule_result.is_empty() {
        Some(rule_result)
    } else {
        None
    }
}

fn apply_rule_operation(
    res_name: &str,
    rule: &structs::Rule,
    rule_val: &str,
    val: &Value,
) -> Option<String> {
    debug!(
        "OpCode::{:?} with rule_val as {} and val as {} of RValueType::{:?}",
        &rule.operation, &rule_val, &val, &rule.rule_vtype
    );
    match rule.operation {
        enums::OpCode::Require => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    if util::format_value(&val) == util::strip_ws_nl(rule_val.to_string()) {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c
                            ),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted value is [{}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            ),
                        })
                    }
                }
                enums::RValueType::Regex => {
                    let re = Regex::new(rule_val).unwrap();
                    if re.is_match(&val.to_string()) {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted pattern is [{}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        })
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
        enums::OpCode::RequireNot => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    if util::format_value(&val) != util::strip_ws_nl(rule_val.to_string()) {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c
                            ),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and that value is not permitted",
                                res_name,
                                &rule.field,
                                util::format_value(&val)
                            ),
                        })
                    }
                }
                enums::RValueType::Regex => {
                    let re = Regex::new(rule_val).unwrap();
                    if !re.is_match(&val.to_string()) {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the pattern [{}] is not permitted",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        })
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
        enums::OpCode::In => {
            let value_vec = util::convert_list_var_to_vec(rule_val);
            let val_as_string = match val.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(val).unwrap(),
            };
            if value_vec.contains(&util::strip_ws_nl(val_as_string)) {
                info!("Result: PASS");
                None
            } else {
                info!("Result: FAIL");
                Some(match &rule.custom_msg {
                    Some(c) => format!(
                        "[{}] failed because [{}] is [{}] and {}",
                        res_name,
                        &rule.field,
                        util::format_value(&val),
                        c
                    ),
                    None => format!(
                        "[{}] failed because [{}] is not in {} for [{}]",
                        res_name,
                        util::format_value(&val),
                        rule_val.to_string(),
                        &rule.field
                    ),
                })
            }
        }
        enums::OpCode::NotIn => {
            let value_vec = util::convert_list_var_to_vec(rule_val);
            let val_as_string = match val.as_str() {
                Some(s) => s.to_string(),
                None => serde_json::to_string(val).unwrap(),
            };
            if !value_vec.contains(&util::strip_ws_nl(val_as_string)) {
                info!("Result: PASS");
                None
            } else {
                info!("Result: FAIL");
                Some(match &rule.custom_msg {
                    Some(c) => format!(
                        "[{}] failed because [{}] is [{}] and {}",
                        res_name,
                        &rule.field,
                        util::format_value(&val),
                        c
                    ),
                    None => format!(
                        "[{}] failed because [{}] is in {} which is not permitted for [{}]",
                        res_name,
                        util::format_value(&val),
                        rule_val.to_string(),
                        &rule.field
                    ),
                })
            }
        }
        enums::OpCode::LessThan => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    let template_val = util::parse_value_as_float(&val);
                    let rule_val = util::parse_str_as_float(rule_val);
                    if template_val < rule_val {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted value is [< {}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        }
                        )
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
        enums::OpCode::GreaterThan => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    let template_val = util::parse_value_as_float(&val);
                    let rule_val = util::parse_str_as_float(rule_val);
                    if template_val > rule_val {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted value is [> {}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        }
                        )
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
        enums::OpCode::LessThanOrEqualTo => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    let template_val = util::parse_value_as_float(&val);
                    let rule_val = util::parse_str_as_float(rule_val);
                    if template_val <= rule_val {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted value is [<= {}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        }
                        )
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
        enums::OpCode::GreaterThanOrEqualTo => {
            match rule.rule_vtype {
                enums::RValueType::Value | enums::RValueType::Variable => {
                    let template_val = util::parse_value_as_float(&val);
                    let rule_val = util::parse_str_as_float(rule_val);
                    if template_val >= rule_val {
                        info!("Result: PASS");
                        None
                    } else {
                        info!("Result: FAIL");
                        Some(match &rule.custom_msg {
                            Some(c) => format!(
                                "[{}] failed because [{}] is [{}] and {}",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                c),
                            None => format!(
                                "[{}] failed because [{}] is [{}] and the permitted value is [>= {}]",
                                res_name,
                                &rule.field,
                                util::format_value(&val),
                                rule_val.to_string()
                            )
                        }
                        )
                    }
                }
                _ => {
                    error!("REQUIRE rule type that doesn't match RValueType of Regex, Variable or Value");
                    None
                }
            }
        }
    }
}
