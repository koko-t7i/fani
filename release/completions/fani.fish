complete -c fani -f
complete -c fani -n '__fish_use_subcommand' -a init -d 'Create a safe starter configuration'
complete -c fani -n '__fish_use_subcommand' -a sync -d 'Translate, verify, materialize, and publish pending work'
complete -c fani -n '__fish_use_subcommand' -a status -d 'Plan from an immutable Git revision'
complete -c fani -n '__fish_use_subcommand' -a check -d 'Run the CI-friendly read-only check'
complete -c fani -n '__fish_use_subcommand' -a doctor -d 'Validate configuration and prerequisites'
complete -c fani -n '__fish_use_subcommand' -a adopt -d 'Adopt divergent human target files'
complete -c fani -n '__fish_use_subcommand' -a discard -d 'Restore canonical verified target files'

for command in init sync status check doctor adopt discard
    complete -c fani -n "__fish_seen_subcommand_from $command" -l config -r -F -d 'Configuration file'
end
complete -c fani -n '__fish_seen_subcommand_from init' -l repo -r -F -d 'Repository path'
complete -c fani -n '__fish_seen_subcommand_from init' -l lang -r -d 'Target language'
complete -c fani -n '__fish_seen_subcommand_from init' -l provider -r -a 'anthropic openai xai deepseek' -d 'Built-in provider'
complete -c fani -n '__fish_seen_subcommand_from init' -l model -r -d 'Provider model'
complete -c fani -n '__fish_seen_subcommand_from init' -l force -d 'Replace an existing configuration'
for command in sync status check adopt discard
    complete -c fani -n "__fish_seen_subcommand_from $command" -l repo -r -d 'Repository path or basename'
    complete -c fani -n "__fish_seen_subcommand_from $command" -l lang -r -d 'Language'
end
complete -c fani -n '__fish_seen_subcommand_from sync' -l report-dir -r -F -d 'Report directory'
complete -c fani -n '__fish_seen_subcommand_from sync' -l quiet -d 'Suppress progress output'
