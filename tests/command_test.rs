mod helpers;

use agman::command::StoredCommand;
use helpers::test_config;

#[test]
fn stored_command_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let yaml = r#"name: Test Command
id: test-cmd
description: A test command
requires_arg: branch
post_action: archive_task

steps:
  - agent: coder
    until: AGENT_DONE
"#;
    let path = config.commands_dir.join("test-cmd.yaml");
    std::fs::write(&path, yaml).unwrap();

    let cmd = StoredCommand::load(&path).unwrap();
    assert_eq!(cmd.name, "Test Command");
    assert_eq!(cmd.id, "test-cmd");
    assert_eq!(cmd.description, "A test command");
    assert_eq!(cmd.requires_arg.as_deref(), Some("branch"));
    assert_eq!(cmd.post_action.as_deref(), Some("archive_task"));
    assert_eq!(cmd.flow_path, path);
}

#[test]
fn stored_command_list_all() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    for (id, name) in [("alpha", "Zulu"), ("beta", "Alpha"), ("gamma", "Mike")] {
        let yaml = format!(
            "name: {}\nid: {}\ndescription: desc\n\nsteps:\n  - agent: coder\n    until: AGENT_DONE\n",
            name, id
        );
        std::fs::write(config.commands_dir.join(format!("{}.yaml", id)), yaml).unwrap();
    }

    let commands = StoredCommand::list_all(&config.commands_dir).unwrap();
    assert_eq!(commands.len(), 3);
    // Sorted by name: Alpha, Mike, Zulu
    assert_eq!(commands[0].name, "Alpha");
    assert_eq!(commands[1].name, "Mike");
    assert_eq!(commands[2].name, "Zulu");
}

#[test]
fn stored_command_get_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let config = test_config(&tmp);
    config.ensure_dirs().unwrap();

    let yaml = "name: Foo\nid: foo\ndescription: foo desc\n\nsteps:\n  - agent: coder\n    until: AGENT_DONE\n";
    std::fs::write(config.commands_dir.join("foo.yaml"), yaml).unwrap();

    let found = StoredCommand::get_by_id(&config.commands_dir, "foo").unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, "foo");

    let not_found = StoredCommand::get_by_id(&config.commands_dir, "nonexistent").unwrap();
    assert!(not_found.is_none());
}
