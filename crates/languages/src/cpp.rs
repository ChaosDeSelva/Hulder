use collections::HashMap;
use gpui::{App, Entity, Task};
use language::{Buffer, ContextProvider};
use settings::SemanticTokenRules;
use task::{TaskTemplate, TaskTemplates, VariableName};

/// Compiles one file into `.hulder/bin/<stem>` and runs it. The task layer
/// joins `command` and `args` into one raw line for the user's shell
/// (`prepare_task_for_spawn` uses `build_no_quote`), so no path may appear
/// on that line. Paths travel in the task's `env` instead: Zed substitutes
/// `$ZED_*` there and hands the values to the process without any shell
/// parsing, and the script only ever expands them inside double quotes. The
/// script is passed single-quoted as one arg, so it must not contain `'`.
const COMPILE_AND_RUN_SCRIPT: &str = r#"mkdir -p "$HULDER_BIN" && "$HULDER_CC" -g -O0 "$HULDER_FILE" -o "$HULDER_BIN/$HULDER_STEM" && exec "$HULDER_BIN/$HULDER_STEM""#;

fn compile_and_run_template(label: &str, compiler: &str, tag: &str) -> TaskTemplate {
    let root = VariableName::WorktreeRoot.template_value();
    let mut env: HashMap<String, String> = HashMap::from_iter([
        ("HULDER_BIN".to_owned(), format!("{root}/.hulder/bin")),
        (
            "HULDER_FILE".to_owned(),
            VariableName::File.template_value(),
        ),
        (
            "HULDER_STEM".to_owned(),
            VariableName::Stem.template_value(),
        ),
        ("HULDER_CC".to_owned(), compiler.to_owned()),
    ]);
    // On macOS the `clang++` shim follows xcode-select, which may point at
    // Command Line Tools without the C++ standard headers. Prefer Xcode.app.
    if cfg!(target_os = "macos") {
        let developer_dir = "/Applications/Xcode.app/Contents/Developer";
        if std::path::Path::new(developer_dir).is_dir() {
            env.insert("DEVELOPER_DIR".to_owned(), developer_dir.to_owned());
        }
    }
    TaskTemplate {
        label: label.to_owned(),
        command: "sh".to_owned(),
        args: vec!["-c".to_owned(), format!("'{COMPILE_AND_RUN_SCRIPT}'")],
        env,
        cwd: Some(root),
        tags: vec![tag.to_owned()],
        ..TaskTemplate::default()
    }
}

pub(crate) struct CFamilyContextProvider;

impl ContextProvider for CFamilyContextProvider {
    fn associated_tasks(&self, _: Option<Entity<Buffer>>, _: &App) -> Task<Option<TaskTemplates>> {
        Task::ready(Some(TaskTemplates(vec![
            compile_and_run_template("C++: compile and run active file", "clang++", "cpp-main"),
            compile_and_run_template("C: compile and run active file", "clang", "c-main"),
        ])))
    }
}

pub(crate) fn semantic_token_rules() -> SemanticTokenRules {
    let content = grammars::get_file("cpp/semantic_token_rules.json")
        .expect("missing cpp/semantic_token_rules.json");
    let json = std::str::from_utf8(&content.data).expect("invalid utf-8 in semantic_token_rules");
    settings::parse_json_with_comments::<SemanticTokenRules>(json)
        .expect("failed to parse cpp semantic_token_rules.json")
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, BorrowAppContext, TestAppContext};
    use language::{AutoindentMode, Buffer};
    use settings::SettingsStore;
    use std::num::NonZeroU32;
    use unindent::Unindent;

    #[test]
    fn compile_and_run_template_uses_task_variables_and_gutter_tag() {
        let template = super::compile_and_run_template(
            "C++: compile and run active file",
            "clang++",
            "cpp-main",
        );
        assert_eq!(template.tags, vec!["cpp-main".to_owned()]);
        assert_eq!(template.command, "sh");
        assert_eq!(template.cwd.as_deref(), Some("$ZED_WORKTREE_ROOT"));
        assert_eq!(template.args.len(), 2, "no path may ride on the shell line");
        let script = &template.args[1];
        assert!(
            script.starts_with('\'') && script.ends_with('\''),
            "{script}"
        );
        assert!(
            !super::COMPILE_AND_RUN_SCRIPT.contains('\''),
            "the script is single-quoted on the shell line, so it may not contain one"
        );
        assert!(
            !script.contains("$ZED_"),
            "script must not interpolate task variables"
        );
        assert_eq!(
            template.env.get("HULDER_BIN").map(String::as_str),
            Some("$ZED_WORKTREE_ROOT/.hulder/bin")
        );
        assert_eq!(
            template.env.get("HULDER_FILE").map(String::as_str),
            Some("$ZED_FILE")
        );
        assert_eq!(
            template.env.get("HULDER_STEM").map(String::as_str),
            Some("$ZED_STEM")
        );
        assert_eq!(
            template.env.get("HULDER_CC").map(String::as_str),
            Some("clang++")
        );
    }

    /// Runs the resolved task through the real spawn path
    /// (`prepare_task_for_spawn`, which joins the args into one raw line for
    /// the user's shell and passes `env` to the process), with a worktree
    /// whose path holds a space, a quote and `$(`. A fake compiler writes a
    /// runnable script so no clang is needed.
    #[test]
    #[cfg(unix)]
    fn compile_and_run_task_survives_hostile_paths() {
        use std::os::unix::fs::PermissionsExt;
        use task::{Shell, TaskContext, TaskVariables, VariableName};
        use terminal_view::terminal_panel::prepare_task_for_spawn;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let worktree = temp_dir.path().join("it's a; $(touch pwned) dir");
        std::fs::create_dir_all(&worktree).expect("create worktree");
        let source = worktree.join("main.cpp");
        std::fs::write(&source, "int main() {}\n").expect("write source");

        let fake_compiler = worktree.join("fake-cc");
        std::fs::write(
            &fake_compiler,
            concat!(
                "#!/bin/sh\n",
                r#"printf '#!/bin/sh\necho compiled-and-ran "$0"\n' > "$5" && chmod +x "$5""#,
                "\n"
            ),
        )
        .expect("write fake compiler");
        std::fs::set_permissions(&fake_compiler, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake compiler");

        let template = super::compile_and_run_template(
            "C++: compile and run active file",
            fake_compiler.to_str().expect("utf-8 path"),
            "cpp-main",
        );
        let context = TaskContext {
            cwd: Some(worktree.clone()),
            task_variables: TaskVariables::from_iter([
                (
                    VariableName::WorktreeRoot,
                    worktree.to_string_lossy().into_owned(),
                ),
                (VariableName::File, source.to_string_lossy().into_owned()),
                (VariableName::Stem, "main".to_owned()),
            ]),
            project_env: Default::default(),
        };
        let resolved = template
            .resolve_task("test", &context)
            .expect("resolved task")
            .resolved;

        let prepared = prepare_task_for_spawn(&resolved, &Shell::System, false);
        let program = prepared.command.expect("shell program");
        // Run from the worktree so a `$(touch pwned)` that did get expanded
        // would land where the assertion below looks.
        let output = smol::block_on(
            util::command::new_command(program)
                .args(prepared.args)
                .envs(prepared.env)
                .current_dir(&worktree)
                .output(),
        )
        .expect("run task");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "stdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("compiled-and-ran"), "stdout: {stdout}");
        assert!(worktree.join(".hulder/bin/main").is_file());
        assert!(!worktree.join("pwned").exists(), "shell expanded $(...)");
    }

    #[gpui::test]
    async fn test_cpp_autoindent_switch_case(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    switch (a) {
                    case 1:
                    b++;
                    break;
                    case 2:
                    case 3:
                    c++;
                    break;
                    default:
                    d++;
                    }
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  switch (a) {
                    case 1:
                      b++;
                      break;
                    case 2:
                    case 3:
                      c++;
                      break;
                    default:
                      d++;
                  }
                }
                "#
                .unindent(),
                "statements under a case label should be indented, and the next label outdented"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_access_specifier(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Foo {
                    public:
                    void bar();
                    private:
                    int x;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Foo {
                  public:
                    void bar();
                  private:
                    int x;
                };
                "#
                .unindent(),
                "members after access specifiers should be indented one level deeper than the specifier"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_access_specifier_next_line(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Foo {
                    public:
                      void bar();
                    void baz();
                    private:
                      int x;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Foo {
                  public:
                    void bar();
                    void baz();
                  private:
                    int x;
                };
                "#
                .unindent(),
                "members after access specifiers should be indented one level deeper than the specifier"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_nested_class_access_specifiers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Outer {
                    public:
                    class Inner {
                    public:
                    void inner_pub();
                    private:
                    int inner_priv;
                    };
                    private:
                    int outer_priv;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Outer {
                  public:
                    class Inner {
                      public:
                        void inner_pub();
                      private:
                        int inner_priv;
                    };
                  private:
                    int outer_priv;
                };
                "#
                .unindent(),
                "nested class access specifiers should indent independently at each nesting level"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_consecutive_access_specifiers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Foo {
                    public:
                    protected:
                    private:
                    int x;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Foo {
                  public:
                  protected:
                  private:
                    int x;
                };
                "#
                .unindent(),
                "consecutive access specifiers with no members between them should all align at class level"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_indented_access_specifiers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Foo {
                    int default_member;
                    public:
                    void pub_method();
                    private:
                    int priv_member;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Foo {
                  int default_member;
                  public:
                    void pub_method();
                  private:
                    int priv_member;
                };
                "#
                .unindent(),
                "access specifiers should be indented one level inside class braces"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_access_specifier_with_method_bodies(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    class Foo {
                    public:
                    void bar() {
                    if (x)
                    y++;
                    }
                    private:
                    int get_x() {
                    return x;
                    }
                    int x;
                    };
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                class Foo {
                  public:
                    void bar() {
                      if (x)
                        y++;
                    }
                  private:
                    int get_x() {
                      return x;
                    }
                    int x;
                };
                "#
                .unindent(),
                "method bodies inside access specifier sections should compose brace and specifier indent"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_basic(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit([(0..0, "int main() {}")], None, cx);

            let ix = buffer.len() - 1;
            buffer.edit([(ix..ix, "\n\n")], Some(AutoindentMode::EachLine), cx);
            assert_eq!(
                buffer.text(),
                "int main() {\n  \n}",
                "content inside braces should be indented"
            );

            buffer
        });
    }

    #[gpui::test]
    async fn test_cpp_autoindent_if_else(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let test_settings = SettingsStore::test(cx);
            cx.set_global(test_settings);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |s| {
                    s.project.all_languages.defaults.tab_size = NonZeroU32::new(2);
                });
            });
        });
        let language = crate::language("cpp", tree_sitter_cpp::LANGUAGE.into());

        cx.new(|cx| {
            let mut buffer = Buffer::local("", cx).with_language(language, cx);

            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a)
                    b;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a)
                    b;
                }
                "#
                .unindent(),
                "body of if-statement without braces should be indented"
            );

            let ix = buffer.len() - 4;
            buffer.edit([(ix..ix, "\n.c")], Some(AutoindentMode::EachLine), cx);
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a)
                    b
                      .c;
                }
                "#
                .unindent(),
                "field expression (.c) should be indented further than the statement body"
            );

            buffer.edit([(0..buffer.len(), "")], Some(AutoindentMode::EachLine), cx);
            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a) a++;
                    else b++;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a) a++;
                  else b++;
                }
                "#
                .unindent(),
                "single-line if/else without braces should align at the same level"
            );

            buffer.edit([(0..buffer.len(), "")], Some(AutoindentMode::EachLine), cx);
            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a)
                    b++;
                    else
                    c++;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a)
                    b++;
                  else
                    c++;
                }
                "#
                .unindent(),
                "multi-line if/else without braces should indent statement bodies"
            );

            buffer.edit([(0..buffer.len(), "")], Some(AutoindentMode::EachLine), cx);
            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a)
                    if (b)
                    c++;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a)
                    if (b)
                      c++;
                }
                "#
                .unindent(),
                "nested if statements without braces should indent properly"
            );

            buffer.edit([(0..buffer.len(), "")], Some(AutoindentMode::EachLine), cx);
            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a)
                    b++;
                    else if (c)
                    d++;
                    else
                    f++;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a)
                    b++;
                  else if (c)
                    d++;
                  else
                    f++;
                }
                "#
                .unindent(),
                "else-if chains should align all conditions at same level with indented bodies"
            );

            buffer.edit([(0..buffer.len(), "")], Some(AutoindentMode::EachLine), cx);
            buffer.edit(
                [(
                    0..0,
                    r#"
                    int main() {
                    if (a) {
                    b++;
                    } else
                    c++;
                    }
                    "#
                    .unindent(),
                )],
                Some(AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(
                buffer.text(),
                r#"
                int main() {
                  if (a) {
                    b++;
                  } else
                    c++;
                }
                "#
                .unindent(),
                "mixed braces should indent properly"
            );

            buffer
        });
    }
}
