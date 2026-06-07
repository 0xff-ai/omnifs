alias ls="ls --color=auto"
alias ll="ls -lrt"

setopt NO_AUTO_CD
setopt PROMPT_SUBST
PROMPT="%F{blue}%~%f %# "
skip_global_compinit=1

# Welcome banner. Only fires for interactive shells, so `zsh -c '...'`
# invocations like `docker exec omnifs zsh -lc 'omnifs status'` stay silent.
if [[ -o interactive ]]; then
    print -P "%F{8}        ·         .              *              ·%f"
    print -P "%F{8}   ⋆                     .                 .%f"
    print -P "%F{8}                                                       *%f"
    print -P "%F{8}              ╔═╗ ╔╦╗ ╔╗╔ ╦ ╔═╗ ╔═╗%f"
    print -P "%F{8}   ·          ║ ║ ║║║ ║║║ ║ ╠╣  ╚═╗               ⋆%f"
    print -P "%F{8}              ╚═╝ ╩ ╩ ╝╚╝ ╩ ╚   ╚═╝%f"
    print -P "%F{8}                                                       .%f"
    print -P "%B%F{7}        *           open a path, read the world.%f%b"
    print -P "%F{8}   ·         ⋆               .              *%f"
    print
    print "omnifs alpha — projected filesystem at /omnifs"
    if command -v omnifs >/dev/null 2>&1; then
        print -P "%F{8}version $(omnifs --version 2>/dev/null | awk '{print $NF}')%f"
    fi
    print
    print "Try these paths:"
    print
    print -P "  %F{8}# clone any repo just by listing it%f"
    print -P "  %F{7}\$ ls /github/0xff-ai/omnifs/repo%f"
    print
    print -P "  %F{8}# resolve any DNS records by catting%f"
    print -P "  %F{7}\$ cat /dns/openai.com/TXT%f"
    print
    print -P "  %F{8}# print titles of recent papers on AI from arXiv%f"
    print -P "  %F{7}\$ find /arxiv/categories/cs.AI/recent/pages/0 -name 'metadata.json' -exec jq -r '.title' {} +%f"
    print
    print "Useful commands:"
    print
    print -P "  %F{8}# mounts, providers, cache, auth%f"
    print -P "  %F{7}\$ omnifs status%f"
    print
    print -P "  %F{8}# follow runtime traces%f"
    print -P "  %F{7}\$ omnifs logs -f%f"
    print
    print -P "  %F{8}# inspect stored credentials%f"
    print -P "  %F{7}\$ omnifs auth list%f"
    print
fi
