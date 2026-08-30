_fani()
{
    local cur prev commands
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    commands="init sync status check doctor adopt discard help"

    case "${prev}" in
        --config|--repo|--lang|--report-dir)
            if [ "${prev}" = "--config" ] || [ "${prev}" = "--report-dir" ]; then
                COMPREPLY=( $(compgen -f -- "${cur}") )
            fi
            return 0
            ;;
    esac

    if [ "${COMP_CWORD}" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "${commands} --help --version" -- "${cur}") )
        return 0
    fi

    case "${COMP_WORDS[1]}" in
        init)
            COMPREPLY=( $(compgen -W "--config --repo --lang --provider --model --force --help" -- "${cur}") )
            ;;
        sync)
            COMPREPLY=( $(compgen -W "--config --repo --lang --report-dir --quiet --help" -- "${cur}") )
            ;;
        status|check|adopt|discard)
            COMPREPLY=( $(compgen -W "--config --repo --lang --help" -- "${cur}") )
            ;;
        doctor)
            COMPREPLY=( $(compgen -W "--config --help" -- "${cur}") )
            ;;
    esac
}
complete -F _fani fani
