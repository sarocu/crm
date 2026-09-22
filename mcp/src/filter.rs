//! Building Meilisearch filter expressions safely.
//!
//! Filter values come from tool arguments, which come from a model, which
//! may have picked them up from a document. Quoting them properly keeps a
//! stray `"` or `AND` from changing the shape of the expression.

/// Accumulates `AND`-joined clauses.
#[derive(Debug, Default, Clone)]
pub struct Filter {
    clauses: Vec<String>,
}

impl Filter {
    /// Start from the clauses every query on an index needs — for
    /// companies, leaving out records folded into another.
    pub fn new(base: &str) -> Self {
        let mut f = Self::default();
        f.and(base);
        f
    }

    pub fn and(&mut self, clause: impl Into<String>) -> &mut Self {
        let clause = clause.into();
        if !clause.is_empty() {
            self.clauses.push(clause);
        }
        self
    }

    /// `field = "value"`.
    pub fn eq(&mut self, field: &str, value: &str) -> &mut Self {
        self.and(format!("{field} = {}", quote(value)))
    }

    /// `field IN ["a", "b"]`. A no-op for an empty list, which otherwise
    /// produces `IN []` and silently matches nothing.
    pub fn any_of(&mut self, field: &str, values: &[String]) -> &mut Self {
        let c = in_clause(field, values);
        self.and(c)
    }

    /// `NOT field IN [...]`, a no-op for an empty list.
    pub fn none_of(&mut self, field: &str, values: &[String]) -> &mut Self {
        let c = in_clause(field, values);
        if c.is_empty() {
            return self;
        }
        self.and(format!("NOT {c}"))
    }

    /// `(field IN [...] OR field NOT EXISTS)`: match, or unknown.
    pub fn any_of_or_missing(&mut self, field: &str, values: &[String]) -> &mut Self {
        let c = in_clause(field, values);
        if c.is_empty() {
            return self;
        }
        // Parenthesised so the OR cannot escape into the other clauses.
        self.and(format!("({c} OR {field} NOT EXISTS)"))
    }

    pub fn gte(&mut self, field: &str, value: i64) -> &mut Self {
        self.and(format!("{field} >= {value}"))
    }

    pub fn lte(&mut self, field: &str, value: i64) -> &mut Self {
        self.and(format!("{field} <= {value}"))
    }

    pub fn build(&self) -> String {
        self.clauses.join(" AND ")
    }
}

fn in_clause(field: &str, values: &[String]) -> String {
    let vals: Vec<String> = values
        .iter()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(quote)
        .collect();
    if vals.is_empty() {
        return String::new();
    }
    format!("{field} IN [{}]", vals.join(", "))
}

/// Quote a value for a Meilisearch filter expression.
pub fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clauses_are_and_joined_and_ors_are_contained() {
        let mut f = Filter::new("merged_into NOT EXISTS");
        f.eq("hq_state", "CO")
            .any_of_or_missing("hq_country", &["US".into()])
            .gte("employees", 50);
        assert_eq!(
            f.build(),
            r#"merged_into NOT EXISTS AND hq_state = "CO" AND (hq_country IN ["US"] OR hq_country NOT EXISTS) AND employees >= 50"#
        );
    }

    #[test]
    fn values_are_quoted_and_escaped() {
        let mut f = Filter::default();
        f.eq("name", "O\"Brien \\ Co");
        assert_eq!(f.build(), r#"name = "O\"Brien \\ Co""#);
    }

    #[test]
    fn an_injected_operator_stays_a_literal_value() {
        let mut f = Filter::default();
        f.eq("status", "new\" OR status = \"qualified");
        assert_eq!(f.build(), r#"status = "new\" OR status = \"qualified""#);
    }

    #[test]
    fn empty_lists_add_no_clause() {
        let mut f = Filter::new("x");
        f.any_of("verticals", &[]);
        f.none_of("id", &["  ".to_string()]);
        f.any_of_or_missing("hq_country", &[]);
        assert_eq!(f.build(), "x");
    }

    #[test]
    fn exclusions_negate_an_in_list() {
        let mut f = Filter::default();
        f.none_of("id", &["a".into(), "b".into()]);
        assert_eq!(f.build(), r#"NOT id IN ["a", "b"]"#);
    }
}
