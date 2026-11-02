"""P4 G6 labelling rules r2 (frozen before the second blind check).
Usage: CTX_REPO=<repo> label.py <telemetry.json> <labels.json>
 (disclosed in docs/gates/p4-g6-audit.md).
Allow (sampled; certificate Valid)        -> ok
Warn on a file write outside the repo     -> false
Warn on a file write in the repo          -> ok
Warn on a shell command                   -> ok only if it changes tracked repository work:
    git add/commit/push/rm/mv/reset/checkout/apply/stash/merge/rebase/tag, sed -i, cargo fmt (not --check),
    or a write (redirect, tee, cp/mv destination, python write) whose target is a repository path
    while the command runs in the repository; otherwise false (read-only, tests, builds, logs to temp,
    work in other directories).
"""
import json,re,sys
import os
REPO=os.environ.get('CTX_REPO', os.getcwd())
REPO_T=REPO.replace(os.path.expanduser('~'),'~',1)  # the same path written with ~
GIT=re.compile(r"\bgit\s+(-C\s+\S+\s+)?(commit|push|add|rm|mv|reset|checkout|apply|stash|merge|rebase|tag)\b")
FMT=re.compile(r"\bcargo\s+fmt\b(?![^;&|]*--check)|\brustfmt\b(?![^;&|]*--check)|\bsed\s+-i|\bperl\s+-p?i")
PYW=re.compile(r"write_text|open\([^)]*,\s*['\"][wa]|\.write\(|\.unlink\(|os\.remove|shutil\.(copy|move|rmtree)")
def strip(c): return re.sub(r'\brtk( proxy)?\s+','',c or '')
def in_repo(t, cwd_repo):
    t=t.strip('\'"')
    if t in('/dev/null','&1','&2') or t.startswith('&') : return False
    if t.startswith('$') or t.startswith('"$') : return False
    if t.startswith(REPO) or t.startswith(REPO_T): return True
    if t.startswith('/') or t.startswith('~'): return False
    return cwd_repo
def split_heredocs(c):
    """(shell lines, heredoc bodies): each body is attached to the line that opened it."""
    lines=c.split('\n'); out=[]; i=0
    while i<len(lines):
        l=lines[i]; m=re.search(r"<<-?\s*['\"]?(\w+)['\"]?",l); body=''
        if m:
            tag=m.group(1); j=i+1; b=[]
            while j<len(lines) and lines[j].strip()!=tag: b.append(lines[j]); j+=1
            body='\n'.join(b); i=j
        out.append((l,body)); i+=1
    return out
def rule(d):
    if d['decision']=='Allow': return 'ok','allow-valid'
    if d['tool']!='Shell':
        paths=re.findall(r'"([^"]*)"',d['paths'])
        if paths and all(p.startswith('/') and not p.startswith(REPO+'/') for p in paths): return 'false','write-outside-repo'
        return 'ok','repo-write'
    c=strip(d['command']); cwd_repo=True
    for line,body in split_heredocs(c):
        for seg in re.split(r"&&|\|\||;|\n",line):
            s=seg.strip()
            m=re.match(r"cd\s+(\S+)",s)
            if m:
                cwd_repo=m.group(1).strip('\'"').rstrip('/') in (REPO,REPO_T,'.'); continue
            if cwd_repo and (GIT.search(s) or FMT.search(s)): return 'ok','shell-repo-change'
            t=re.findall(r"(?<![0-9&>=<-])>{1,2}(?!=)\s*([^\s;&|)]+)",s)+re.findall(r"\btee\s+(?:-a\s+)?([^\s;&|]+)",s)
            t+=re.findall(r"^(?:cp|mv)\s+(?:-\w+\s+)*\S+\s+([^\s;&|]+)",s)
            if any(in_repo(x,cwd_repo) for x in t): return 'ok','shell-repo-change'
        if body and PYW.search(body):
            q=re.findall(r"['\"]([\w./~-]+\.(?:rs|md|toml|json|js|css|html|py|sh|yml|yaml|txt|lock))['\"]",body)
            if any(in_repo(x,cwd_repo) for x in q): return 'ok','shell-repo-change'
    return 'false','shell-no-repo-change'
if __name__=='__main__':
    T=json.load(open(sys.argv[1]))
    json.dump({d['id']:rule(d) for d in T if d['kind']=='Gate'},open(sys.argv[2],'w'))
