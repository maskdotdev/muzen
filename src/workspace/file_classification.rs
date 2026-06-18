use std::path::Path;

pub(crate) fn is_textish(path: &Path) -> bool {
    if matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("Dockerfile")
            | Some("Gemfile")
            | Some("Rakefile")
            | Some("Procfile")
            | Some("Makefile")
            | Some("Brewfile")
            | Some("Podfile")
            | Some("Cartfile")
            | Some("Pipfile")
            | Some("poetry.lock")
            | Some("package-lock.json")
            | Some("yarn.lock")
            | Some("pnpm-lock.yaml")
            | Some("Gemfile.lock")
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "bash"
                | "c"
                | "cc"
                | "cfg"
                | "cjs"
                | "clj"
                | "cpp"
                | "cs"
                | "css"
                | "csv"
                | "cts"
                | "dart"
                | "diff"
                | "dockerfile"
                | "erb"
                | "ex"
                | "exs"
                | "go"
                | "graphql"
                | "h"
                | "hpp"
                | "html"
                | "java"
                | "js"
                | "json"
                | "jsx"
                | "kt"
                | "kts"
                | "less"
                | "lock"
                | "lua"
                | "mjs"
                | "md"
                | "mdx"
                | "mts"
                | "php"
                | "pl"
                | "proto"
                | "py"
                | "rake"
                | "rb"
                | "rs"
                | "sass"
                | "scala"
                | "scss"
                | "sh"
                | "sql"
                | "swift"
                | "thor"
                | "toml"
                | "ts"
                | "tsx"
                | "txt"
                | "vue"
                | "xml"
                | "yaml"
                | "yml",
        )
    )
}

#[cfg(test)]
mod tests {
    use super::is_textish;
    use std::path::Path;

    #[test]
    fn treats_common_source_and_config_files_as_text() {
        for path in [
            "Gemfile",
            "Gemfile.lock",
            "app/models/topic_embed.rb",
            "app/views/embed/loading.html.erb",
            "app/assets/stylesheets/embed.css.scss",
            "lib/tasks/disqus.thor",
            "config/site_settings.yml",
        ] {
            assert!(is_textish(Path::new(path)), "{path} should be text");
        }
    }
}
