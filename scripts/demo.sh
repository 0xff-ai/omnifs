#!/usr/bin/env zsh
# omnifs demo: the image bakes this script to `/tmp/demo.sh`.
# In the local dev container, the repo copy is also bind-mounted at `/tmp/demo.sh`
# so you can iterate on the script without rebuilding the image.

alias ls="ls --color=always"

bat() {
    batcat --style=plain --paging=never "$@"
    local rc=$?
    print
    return $rc
}

C_AQUA=$'\033[1;38;2;131;192;146m'
C_RESET=$'\033[0m'

show_prompt() { print -nP "%F{yellow}%/%f\n%F{cyan}>%f " }

trace_command() {
    local cmd=$1
    local trace_file=${OMNIFS_CMD_TRACE:-/tmp/omnifs-cmd.trace}
    local start_ns=$(date +%s%N)
    print -r -- "START ${start_ns} ${PWD} ${cmd}" >> "$trace_file"
    eval $cmd
    local rc=$?
    local end_ns=$(date +%s%N)
    print -r -- "END ${end_ns} ${rc} $(( (end_ns - start_ns) / 1000000 ))ms ${cmd}" >> "$trace_file"
    return $rc
}

type_and_run() {
    local cmd=$1
    local prev=""
    for (( i=1; i<=${#cmd}; i++ )); do
        local c=${cmd[$i]}
        print -rn -- "$c"
        if [[ $c == " " || $c == "|" || $c == "/" ]]; then
            sleep 0.$(( RANDOM % 8 + 8 ))
        elif [[ $prev == " " ]]; then
            sleep 0.0$(( RANDOM % 30 + 50 ))
        elif [[ $c == $prev ]]; then
            sleep 0.0$(( RANDOM % 10 + 15 ))
        else
            sleep 0.0$(( RANDOM % 20 + 25 ))
        fi
        prev=$c
    done
    sleep 0.2
    print
    trace_command "$cmd"
    show_prompt
    sleep ${2:-0.6}
}

type_and_run_fast() {
    local cmd=$1
    for (( i=1; i<=${#cmd}; i++ )); do
        print -rn -- "${cmd[$i]}"
        sleep 0.0$(( RANDOM % 5 + 8 ))
    done
    sleep 1.5
    print
    trace_command "$cmd"
    show_prompt
    sleep ${2:-0.6}
}

act() {
    clear
    print
    local lines=("${(@f)$(gum style --bold --border rounded --margin "1 2" --padding "2 4" --border-foreground 142 --foreground 142 "$1")}")
    for line in "${lines[@]}"; do
        print -r -- "$line"
        sleep 0.05
    done
    print
    show_prompt
    sleep 1
}

pick_first_child() {
    local dir=$1
    local attempt=""
    local child=""

    for attempt in {1..20}; do
        child=$(command ls -1 "$dir" 2>/dev/null | head -n 1)
        if [[ -n ${child} ]]; then
            print -r -- "$child"
            return 0
        fi
        sleep 0.2
    done

    return 1
}

pick_first_child_with_file() {
    local dir=$1
    local wanted=$2
    local child=""
    local attempt=""

    for attempt in {1..20}; do
        while IFS= read -r child; do
            [[ -z ${child} ]] && continue
            if [[ -f "${dir}/${child}/${wanted}" ]]; then
                print -r -- "$child"
                return 0
            fi
        done < <(command ls -1 "$dir" 2>/dev/null)

        sleep 0.2
    done

    child=$(pick_first_child "$dir")
    if [[ -n ${child} ]]; then
        print -r -- "$child"
        return 0
    fi

    return 1
}

run_smoke_demo() {
    set -euo pipefail

    local owner=${OMNIFS_DEMO_OWNER:-0xff-ai}
    local requested_repo=${OMNIFS_DEMO_REPO:-omnifs}
    local owner_root="/github/${owner}"
    local requested_repo_root="${owner_root}/${requested_repo}"
    local repo_root=""

    print -r -- "omnifs smoke demo: ${requested_repo_root}"

    for _ in {1..30}; do
        if cd "${requested_repo_root}" 2>/dev/null; then
            repo_root=$PWD
            break
        fi

        if [[ -d ${requested_repo_root}/issues/open && -d ${requested_repo_root}/pulls/open ]]; then
            repo_root=${requested_repo_root}
            break
        fi

        if [[ -d ${owner_root}/issues && -d ${owner_root}/pulls ]]; then
            repo_root=${owner_root}
            break
        fi

        sleep 1
    done

    [[ -n ${repo_root} ]]
    [[ -d ${repo_root}/issues ]]

    cd "${repo_root}"
    ls

    cd "${repo_root}/issues/open"
    ls
    local first_issue
    first_issue=$(command ls -1 | head -n 1)
    if [[ -n $first_issue ]]; then
        cd "$first_issue"
        bat title
    fi

    cd "${repo_root}/pulls/open"
    ls
    local first_pr
    first_pr=$(command ls -1 | head -n 1)
    if [[ -n $first_pr ]]; then
        cd "$first_pr"
        bat title
    fi

    if cd "${repo_root}/actions/runs" 2>/dev/null; then
        ls
        local first_run
        first_run=$(command ls -1 | head -n 1)
        if [[ -n $first_run ]]; then
            cd "$first_run"
            [[ -f status ]] && bat status
            [[ -f conclusion ]] && bat conclusion
        fi
    fi
}

if [[ ${OMNIFS_DEMO_MODE:-full} == smoke ]]; then
    run_smoke_demo
    exit 0
fi

demo_owner=${OMNIFS_DEMO_OWNER:-0xff-ai}
demo_repo=${OMNIFS_DEMO_REPO:-omnifs}

clear
sleep 1

# act 1: navigation

act "omnifs: the universe, mounted on your filesystem

Plan 9 was right. the filesystem was always the right abstraction.
they just didn't have the APIs worth mounting yet.

every service on Earth, mounted as files.
starting with GitHub. cd into any repo.

for humans. for agents."

type_and_run "cd /github" 0.3
type_and_run "ls -lrt" 0
type_and_run "# nothing here... yet!" 0.5
type_and_run "cd ${demo_owner}" 0.3
type_and_run "ls" 2

# act 2: source code

act "source code, mounted as a real tree
the repo materializes lazily on first access. open files directly."

type_and_run "cd ${demo_repo}" 0.1
type_and_run "ls"
type_and_run "cd repo" 0.3
type_and_run "ls" 0.8
type_and_run "bat README.md | head -40" 1
type_and_run_fast "cd .."

# act 3: issues

act "issues are just files
cat a title. grep a thousand bodies. ripgrep across everything."

type_and_run "cd issues" 0.5
type_and_run "ls" 0.6
type_and_run "cd open" 0.3
type_and_run "ls" 0.8
issue_open_dir="${PWD}"
issue_with_body=$(pick_first_child_with_file "$issue_open_dir" body)
[[ -n ${issue_with_body} ]]
type_and_run "cd ${issue_with_body}" 0.3
type_and_run "bat title" 0.5
if [[ -f body ]]; then
    type_and_run "bat -l md body" 1.5
fi
type_and_run "cd .." 0.3
type_and_run_fast 'rg -in memory */title */body --heading --color=always' 2
type_and_run_fast 'cd ../..'

# act 4: pull requests

act "Pull requests are just files too
read the diff. check the state. it is all just text."

type_and_run "cd pulls" 0.5
type_and_run "ls" 0.5
type_and_run "cd open" 0.3
type_and_run "ls" 0.8
pr_open_dir="${PWD}"
pr_with_diff=$(pick_first_child "$pr_open_dir")
[[ -n ${pr_with_diff} ]]
type_and_run "cd ${pr_with_diff}" 0.3
type_and_run "ls" 0.5
type_and_run "bat title" 0.5
type_and_run "bat state" 0.3
type_and_run "bat diff" 2
type_and_run_fast "cd ../../.."

# act 5: CI

act "even GitHub Actions runs
why open a browser when you can cat a CI log directly?"

type_and_run "cd actions" 0.3
type_and_run "ls" 0.5
type_and_run "cd runs" 0.3
type_and_run "ls" 0.8
run_dir="${PWD}"
first_run=$(pick_first_child "$run_dir")
[[ -n ${first_run} ]]
type_and_run "cd ${first_run}" 0.3
type_and_run "ls" 0.8
type_and_run "bat status" 0.3
type_and_run "bat conclusion" 0.5
type_and_run "bat log | head -1000" 2

# outro

clear
gum style --bold --border double --padding "1 3" --border-foreground 142 --foreground 142 \
    "omnifs" \
    "" \
    "the universe, mounted on your filesystem." \
    "" \
    "no API. no SDK. no MCP. no CLI. just files." \
    "if your agent can read a file, it already speaks everything."
sleep 3
