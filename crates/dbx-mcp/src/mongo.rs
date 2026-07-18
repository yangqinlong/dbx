use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum MongoCommand {
    Version,
    Find { collection: String, filter: String, projection: Option<String>, sort: Option<String>, skip: u64, limit: i64 },
    Count { collection: String, filter: String, accurate: bool },
    Aggregate { collection: String, pipeline: String, options: Option<String> },
    Distinct { collection: String, field: String, filter: Option<String> },
    GetIndexes { collection: String },
    CollectionStats { collection: String, metric: String, scale: Option<serde_json::Number> },
    Insert { collection: String, documents: String },
    Update { collection: String, filter: String, update: String, options: Option<String>, many: bool },
    Delete { collection: String, filter: String, many: bool },
    CreateIndex { collection: String, keys: String, options: Option<String> },
    DropIndexes { collection: String, indexes: Option<String>, single: bool },
    DropCollection { collection: String },
}

impl MongoCommand {
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::Insert { .. }
                | Self::Update { .. }
                | Self::Delete { .. }
                | Self::CreateIndex { .. }
                | Self::DropIndexes { .. }
                | Self::DropCollection { .. }
        ) || matches!(self, Self::Aggregate { pipeline, .. } if aggregate_writes(pipeline))
    }

    pub fn is_dangerous(&self) -> bool {
        matches!(self, Self::DropCollection { .. })
            || matches!(self, Self::DropIndexes { indexes: None, single: false, .. })
            || matches!(self, Self::Aggregate { pipeline, .. } if aggregate_writes(pipeline))
    }

    pub fn has_empty_filter(&self) -> bool {
        match self {
            Self::Update { filter, .. } | Self::Delete { filter, .. } => is_empty_object(filter),
            _ => false,
        }
    }
}

pub fn parse(input: &str) -> Result<MongoCommand, String> {
    let source = input.trim().trim_end_matches(';').trim();
    if source.eq_ignore_ascii_case("db.version()") {
        return Ok(MongoCommand::Version);
    }
    let (collection, prefix_end) = parse_collection_prefix(source)?;

    if let Some((args, tail)) = method_call(source, prefix_end, "find") {
        let filter = normalized_json(args.first().map(String::as_str).unwrap_or("{}"))?;
        let projection =
            if args.get(1).is_some_and(|arg| !arg.trim().is_empty()) { Some(normalized_json(&args[1])?) } else { None };
        if args.len() > 2 {
            return Err("MongoDB find() accepts at most filter and projection arguments.".to_string());
        }
        let mut sort = None;
        let mut skip = 0;
        let mut limit = 100;
        for (name, call_args) in chained_calls(&tail)? {
            match name.as_str() {
                "sort" => sort = Some(normalized_json(call_args.first().map(String::as_str).unwrap_or("{}"))?),
                "skip" => skip = parse_integer(&call_args, "skip")? as u64,
                "limit" => limit = parse_integer(&call_args, "limit")?,
                "count" if call_args.is_empty() => {
                    return Ok(MongoCommand::Count { collection, filter, accurate: false });
                }
                _ => return Err(format!("Unsupported MongoDB find() chain: {name}()")),
            }
        }
        return Ok(MongoCommand::Find { collection, filter, projection, sort, skip, limit });
    }

    for (method, accurate) in [("countDocuments", true), ("count", false)] {
        if let Some((args, tail)) = method_call(source, prefix_end, method) {
            if !tail.is_empty() || args.len() > 1 {
                return Err(format!("Invalid MongoDB {method}() command."));
            }
            return Ok(MongoCommand::Count {
                collection,
                filter: normalized_json(args.first().map(String::as_str).unwrap_or("{}"))?,
                accurate,
            });
        }
    }

    if let Some((args, tail)) = method_call(source, prefix_end, "aggregate") {
        if !tail.is_empty() || !(1..=2).contains(&args.len()) {
            return Err("Invalid MongoDB aggregate() command.".to_string());
        }
        let pipeline = normalized_json(&args[0])?;
        if !parse_json_value(&pipeline).is_some_and(|value| value.is_array()) {
            return Err("MongoDB aggregate() requires a pipeline array.".to_string());
        }
        let options = args.get(1).filter(|arg| !arg.trim().is_empty()).map(|arg| normalized_json(arg)).transpose()?;
        return Ok(MongoCommand::Aggregate { collection, pipeline, options });
    }

    if let Some((args, tail)) = method_call(source, prefix_end, "distinct") {
        if !tail.is_empty() || !(1..=2).contains(&args.len()) {
            return Err("Invalid MongoDB distinct() command.".to_string());
        }
        let field = parse_string_arg(&args[0])?;
        let filter = args.get(1).filter(|arg| !arg.trim().is_empty()).map(|arg| normalized_json(arg)).transpose()?;
        return Ok(MongoCommand::Distinct { collection, field, filter });
    }

    if let Some((args, tail)) = method_call(source, prefix_end, "getIndexes") {
        if !tail.is_empty() || !args.is_empty() {
            return Err("Invalid MongoDB getIndexes() command.".to_string());
        }
        return Ok(MongoCommand::GetIndexes { collection });
    }

    for metric in ["stats", "dataSize", "storageSize", "totalIndexSize"] {
        if let Some((args, tail)) = method_call(source, prefix_end, metric) {
            if !tail.is_empty() || args.len() > 1 {
                return Err(format!("Invalid MongoDB {metric}() command."));
            }
            let scale = args
                .first()
                .filter(|arg| !arg.trim().is_empty())
                .map(|arg| {
                    arg.trim()
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .ok_or_else(|| format!("Invalid {metric} scale."))
                })
                .transpose()?;
            return Ok(MongoCommand::CollectionStats { collection, metric: metric.to_string(), scale });
        }
    }

    if let Some((args, tail)) = method_call(source, prefix_end, "insertOne") {
        if !tail.is_empty() || args.len() != 1 {
            return Err("Invalid MongoDB insertOne() command.".to_string());
        }
        return Ok(MongoCommand::Insert { collection, documents: normalized_json(&args[0])? });
    }
    if let Some((args, tail)) = method_call(source, prefix_end, "insertMany") {
        if !tail.is_empty() || args.len() != 1 {
            return Err("Invalid MongoDB insertMany() command.".to_string());
        }
        let documents = normalized_json(&args[0])?;
        if !parse_json_value(&documents).is_some_and(|value| value.is_array()) {
            return Err("MongoDB insertMany() requires an array.".to_string());
        }
        return Ok(MongoCommand::Insert { collection, documents });
    }

    for (method, many) in [("updateOne", false), ("updateMany", true)] {
        if let Some((args, tail)) = method_call(source, prefix_end, method) {
            if !tail.is_empty() || !(2..=3).contains(&args.len()) {
                return Err(format!("Invalid MongoDB {method}() command."));
            }
            return Ok(MongoCommand::Update {
                collection,
                filter: normalized_json(&args[0])?,
                update: normalized_json(&args[1])?,
                options: args
                    .get(2)
                    .filter(|arg| !arg.trim().is_empty())
                    .map(|arg| normalized_json(arg))
                    .transpose()?,
                many,
            });
        }
    }
    for (method, many) in [("deleteOne", false), ("deleteMany", true)] {
        if let Some((args, tail)) = method_call(source, prefix_end, method) {
            if !tail.is_empty() || args.len() != 1 {
                return Err(format!("Invalid MongoDB {method}() command."));
            }
            return Ok(MongoCommand::Delete { collection, filter: normalized_json(&args[0])?, many });
        }
    }
    if let Some((args, tail)) = method_call(source, prefix_end, "createIndex") {
        if !tail.is_empty() || !(1..=2).contains(&args.len()) {
            return Err("Invalid MongoDB createIndex() command.".to_string());
        }
        return Ok(MongoCommand::CreateIndex {
            collection,
            keys: normalized_json(&args[0])?,
            options: args.get(1).filter(|arg| !arg.trim().is_empty()).map(|arg| normalized_json(arg)).transpose()?,
        });
    }
    if let Some((args, tail)) = method_call(source, prefix_end, "dropIndex") {
        if !tail.is_empty() || args.len() != 1 {
            return Err("Invalid MongoDB dropIndex() command.".to_string());
        }
        return Ok(MongoCommand::DropIndexes { collection, indexes: Some(normalized_json(&args[0])?), single: true });
    }
    if let Some((args, tail)) = method_call(source, prefix_end, "dropIndexes") {
        if !tail.is_empty() || args.len() > 1 {
            return Err("Invalid MongoDB dropIndexes() command.".to_string());
        }
        return Ok(MongoCommand::DropIndexes {
            collection,
            indexes: args.first().filter(|arg| !arg.trim().is_empty()).map(|arg| normalized_json(arg)).transpose()?,
            single: false,
        });
    }
    if let Some((args, tail)) = method_call(source, prefix_end, "drop") {
        if !tail.is_empty() || !args.is_empty() {
            return Err("Invalid MongoDB drop() command.".to_string());
        }
        return Ok(MongoCommand::DropCollection { collection });
    }

    Err("Unsupported MongoDB shell command.".to_string())
}

fn parse_collection_prefix(source: &str) -> Result<(String, usize), String> {
    if !source.get(..3).is_some_and(|prefix| prefix.eq_ignore_ascii_case("db.")) {
        return Err("MongoDB command must start with db.<collection>.".to_string());
    }
    let rest = &source[3..];
    if rest.starts_with("getCollection") {
        let open = rest.find('(').ok_or("Invalid db.getCollection() command.")?;
        let close = matching_paren(rest, open).ok_or("Invalid db.getCollection() command.")?;
        let args = split_top_level(&rest[open + 1..close]);
        if args.len() != 1 {
            return Err("db.getCollection() requires one collection name.".to_string());
        }
        let collection = parse_string_arg(&args[0])?;
        let end = 3 + close + 1;
        let suffix = &source[end..];
        let trimmed = suffix.trim_start();
        if !trimmed.starts_with('.') {
            return Err("MongoDB collection method is required.".to_string());
        }
        return Ok((collection, end + suffix.len() - trimmed.len()));
    }
    let collection_end = rest
        .char_indices()
        .find_map(|(index, ch)| (ch == '.' || ch.is_whitespace()).then_some(index))
        .ok_or("MongoDB collection method is required.")?;
    let collection = &rest[..collection_end];
    if collection.is_empty() {
        return Err("Invalid MongoDB collection name.".to_string());
    }
    let suffix = &rest[collection_end..];
    let dot = suffix.find('.').ok_or("MongoDB collection method is required.")?;
    if !suffix[..dot].trim().is_empty() {
        return Err("Invalid MongoDB collection name.".to_string());
    }
    Ok((collection.to_string(), 3 + collection_end + dot))
}

fn method_call(source: &str, prefix_end: usize, method: &str) -> Option<(Vec<String>, String)> {
    let raw_suffix = &source[prefix_end..];
    let suffix = raw_suffix.trim_start();
    let whitespace = raw_suffix.len() - suffix.len();
    let expected = format!(".{method}");
    if !suffix.starts_with(&expected) || !suffix[expected.len()..].starts_with('(') {
        return None;
    }
    let open = prefix_end + whitespace + expected.len();
    let close = matching_paren(source, open)?;
    Some((split_top_level(&source[open + 1..close]), source[close + 1..].trim().to_string()))
}

fn chained_calls(chain: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut rest = chain.trim();
    let mut calls = Vec::new();
    while !rest.is_empty() {
        let Some(rest_after_dot) = rest.strip_prefix('.') else {
            return Err("Invalid MongoDB method chain.".to_string());
        };
        let open = rest_after_dot.find('(').ok_or("Invalid MongoDB method chain.")?;
        let name = rest_after_dot[..open].trim().to_string();
        let close = matching_paren(rest_after_dot, open).ok_or("Invalid MongoDB method chain.")?;
        calls.push((name, split_top_level(&rest_after_dot[open + 1..close])));
        rest = rest_after_dot[close + 1..].trim();
    }
    Ok(calls)
}

fn parse_integer(args: &[String], name: &str) -> Result<i64, String> {
    if args.len() != 1 {
        return Err(format!("MongoDB {name}() requires one integer."));
    }
    let value =
        args[0].trim().parse::<i64>().map_err(|_| format!("MongoDB {name}() requires a non-negative integer."))?;
    if value < 0 {
        return Err(format!("MongoDB {name}() requires a non-negative integer."));
    }
    Ok(value)
}

fn parse_string_arg(arg: &str) -> Result<String, String> {
    let value = parse_json_value(&normalized_json(arg)?).ok_or("Invalid MongoDB string argument.")?;
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| "MongoDB argument must be a string.".to_string())
}

fn normalized_json(input: &str) -> Result<String, String> {
    let transformed = transform_shell_constructors(input.trim())?;
    let value: Value =
        json5::from_str(&transformed).map_err(|error| format!("Invalid MongoDB JSON argument: {error}"))?;
    serde_json::to_string(&value).map_err(|error| error.to_string())
}

fn transform_shell_constructors(input: &str) -> Result<String, String> {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let rest = &input[index..];
        let constructor = if rest.starts_with("ObjectId(") {
            Some("ObjectId(")
        } else if rest.starts_with("ISODate(") {
            Some("ISODate(")
        } else {
            None
        };
        let Some(constructor) = constructor else {
            let ch = rest.chars().next().ok_or("Invalid MongoDB argument.")?;
            output.push(ch);
            index += ch.len_utf8();
            continue;
        };
        let open = index + constructor.len() - 1;
        let close = matching_paren(input, open).ok_or("Unclosed MongoDB value constructor.")?;
        let inner = input[open + 1..close].trim();
        let value = parse_string_arg(inner)?;
        let key = if constructor.starts_with("ObjectId") { "$oid" } else { "$date" };
        output.push_str(&format!("{{\"{key}\":{}}}", serde_json::to_string(&value).unwrap()));
        index = close + 1;
    }
    Ok(output)
}

fn parse_json_value(value: &str) -> Option<Value> {
    serde_json::from_str(value).ok()
}

fn is_empty_object(value: &str) -> bool {
    parse_json_value(value).is_some_and(|value| value.as_object().is_some_and(|object| object.is_empty()))
}

fn aggregate_writes(pipeline: &str) -> bool {
    parse_json_value(pipeline).is_some_and(|value| {
        value.as_array().is_some_and(|stages| {
            stages.iter().any(|stage| {
                stage
                    .as_object()
                    .is_some_and(|object| object.keys().any(|key| matches!(key.as_str(), "$out" | "$merge")))
            })
        })
    })
}

fn matching_paren(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0;
    let mut quote = None;
    let mut escape = false;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        let ch = *byte as char;
        if escape {
            escape = false;
            continue;
        }
        if quote.is_some() {
            if ch == '\\' {
                escape = true;
            } else if Some(ch) == quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            quote = Some(ch);
        } else if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn split_top_level(source: &str) -> Vec<String> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0;
    let mut quote = None;
    let mut escape = false;
    for (index, byte) in source.as_bytes().iter().enumerate() {
        let ch = *byte as char;
        if escape {
            escape = false;
            continue;
        }
        if quote.is_some() {
            if ch == '\\' {
                escape = true;
            } else if Some(ch) == quote {
                quote = None;
            }
            continue;
        }
        if ch == '\'' || ch == '"' || ch == '`' {
            quote = Some(ch);
        } else if matches!(ch, '(' | '[' | '{') {
            depth += 1;
        } else if matches!(ch, ')' | ']' | '}') {
            depth -= 1;
        } else if ch == ',' && depth == 0 {
            result.push(source[start..index].trim().to_string());
            start = index + 1;
        }
    }
    result.push(source[start..].trim().to_string());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_find_with_compass_syntax_and_chain() {
        assert_eq!(
            parse("db.products.find({_id: ObjectId('507f1f77bcf86cd799439011')}, {title: 1, _id: 0}).sort({title: 1}).limit(1)").unwrap(),
            MongoCommand::Find {
                collection: "products".to_string(),
                filter: r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"}}"#.to_string(),
                projection: Some(r#"{"title":1,"_id":0}"#.to_string()),
                sort: Some(r#"{"title":1}"#.to_string()),
                skip: 0,
                limit: 1,
            }
        );
    }

    #[test]
    fn parses_get_collection_and_count() {
        assert_eq!(
            parse("db.getCollection('audit.logs').count()").unwrap(),
            MongoCommand::Count { collection: "audit.logs".to_string(), filter: "{}".to_string(), accurate: false }
        );
    }

    #[test]
    fn identifies_dangerous_aggregate_and_empty_writes() {
        let aggregate = parse(r#"db.projects.aggregate([{"$out":"backup"}])"#).unwrap();
        assert!(aggregate.is_mutating());
        assert!(aggregate.is_dangerous());
        let update = parse("db.projects.updateMany({}, {$set: {active: false}})").unwrap();
        assert!(update.has_empty_filter());
    }

    #[test]
    fn accepts_multiline_chains_and_update_options() {
        let command = parse(
            r#"db.getCollection("operation_logs")
              .find({_id: ObjectId("68ad51ca84c8127bc7d44cb3")})
              .sort({ts: -1})
              .skip(5)
              .limit(10)"#,
        )
        .unwrap();
        assert!(matches!(command, MongoCommand::Find { skip: 5, limit: 10, .. }));

        let update = parse(
            r#"db.orders.updateMany({status: "open"}, {$set: {"items.$[item].status": "done"}}, {arrayFilters: [{"item.id": 7}]})"#,
        )
        .unwrap();
        assert!(matches!(update, MongoCommand::Update { many: true, options: Some(_), .. }));
    }

    #[test]
    fn accepts_stats_and_rejects_negative_pagination() {
        assert!(matches!(
            parse("db.users.stats(1024)").unwrap(),
            MongoCommand::CollectionStats { metric, scale: Some(_), .. } if metric == "stats"
        ));
        assert!(parse("db.users.find({}).skip(-1)").is_err());
    }
}
