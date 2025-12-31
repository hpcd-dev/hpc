use clap::ArgMatches;
use proto::{SubmitPathFilterAction, SubmitPathFilterRule};

pub fn submit_filters_from_matches(matches: &ArgMatches) -> Vec<SubmitPathFilterRule> {
    let Some(("submit", sub_matches)) = matches.subcommand() else {
        return Vec::new();
    };

    let mut ordered: Vec<(usize, SubmitPathFilterAction, String)> = Vec::new();
    let mut push_rules = |arg: &str, action: SubmitPathFilterAction| {
        let values: Vec<String> = sub_matches
            .get_many::<String>(arg)
            .map(|vals| vals.map(|v| v.to_string()).collect())
            .unwrap_or_default();
        let indices: Vec<usize> = sub_matches
            .indices_of(arg)
            .map(|vals| vals.collect())
            .unwrap_or_default();
        for (idx, pattern) in indices.into_iter().zip(values.into_iter()) {
            ordered.push((idx, action, pattern));
        }
    };

    push_rules("include", SubmitPathFilterAction::Include);
    push_rules("exclude", SubmitPathFilterAction::Exclude);

    ordered.sort_by_key(|(idx, _, _)| *idx);
    ordered
        .into_iter()
        .map(|(_, action, pattern)| SubmitPathFilterRule {
            action: action as i32,
            pattern,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Cli;
    use clap::CommandFactory;

    #[test]
    fn submit_filters_preserve_flag_order() {
        let matches = Cli::command().get_matches_from([
            "hpc",
            "submit",
            "cluster-a",
            "./project",
            "--include",
            "a",
            "--exclude",
            "b",
            "--include",
            "c",
        ]);
        let filters = submit_filters_from_matches(&matches);
        let patterns: Vec<&str> = filters.iter().map(|f| f.pattern.as_str()).collect();
        let actions: Vec<i32> = filters.iter().map(|f| f.action).collect();

        assert_eq!(patterns, vec!["a", "b", "c"]);
        assert_eq!(
            actions,
            vec![
                SubmitPathFilterAction::Include as i32,
                SubmitPathFilterAction::Exclude as i32,
                SubmitPathFilterAction::Include as i32
            ]
        );
    }
}
