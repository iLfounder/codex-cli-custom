use pretty_assertions::assert_eq;

use super::plugin_hook_command;

#[test]
fn hook_command_shell_escapes_exact_direct_argv() {
    let command = vec![
        "/tmp/plugin runner".to_string(),
        "--message=two words".to_string(),
        "semi;colon".to_string(),
        "$HOME".to_string(),
        String::new(),
    ];

    assert_eq!(
        plugin_hook_command(&command),
        "'/tmp/plugin runner' '--message=two words' 'semi;colon' '$HOME' ''"
    );
}
