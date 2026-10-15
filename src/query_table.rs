use crate::platform_client::QueryResponse;

pub(crate) struct QueryTable<'a> {
    result: &'a QueryResponse,
}

impl<'a> QueryTable<'a> {
    pub(crate) fn new(result: &'a QueryResponse) -> Self {
        Self { result }
    }

    pub(crate) fn print(&self) {
        let columns = &self.result.columns;
        let rows = &self.result.rows;
        if columns.is_empty() {
            println!("Query executed successfully. No rows returned.");
            return;
        }

        let mut widths: Vec<usize> = columns.iter().map(|c| c.name.len()).collect();
        for row in rows {
            for (i, val) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(format_value(val).len());
                }
            }
        }

        let header: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:width$}", c.name, width = widths[i]))
            .collect();
        println!(" {} ", header.join(" | "));

        let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        println!("-{}-", sep.join("-+-"));

        for row in rows {
            let cells: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let width = widths.get(i).copied().unwrap_or(0);
                    format!("{:width$}", format_value(v), width = width)
                })
                .collect();
            println!(" {} ", cells.join(" | "));
        }

        println!(
            "({} {})",
            rows.len(),
            if rows.len() == 1 { "row" } else { "rows" }
        );
    }
}

fn format_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}
