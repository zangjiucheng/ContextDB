import re,json,sys,datetime
S=sys.argv[1]
out=[]
for line in open(S+'/tel.txt'):
    id_,rest=line.split(' ',1)
    kind=rest.split(' ',1)[0]
    d={'id':id_,'kind':kind}
    m=re.search(r'session: \[([0-9, ]+)\]',rest)
    if m: d['session']=bytes(int(x) for x in m.group(1).split(',')).hex()[:8]
    m=re.search(r'agent: (None|Some\("([^"]*)"\))',rest); d['agent']=m.group(2) if m else None
    if kind=='Gate':
        d['tool']=re.search(r'tool: (\w+)',rest).group(1)
        d['paths']=re.search(r'paths: \[(.*?)\], command',rest).group(1)
        m=re.search(r'command: (None|Some\((".*?")\)), \}?',rest)
        cm=re.search(r'command: Some\((".*)\) \}, mode',rest)
        d['command']=json.loads(cm.group(1)) if cm else None
        for k in ['mode','decision','available','epoch','at']:
            m=re.search(k+r': ([\w()]+)',rest.split('}, mode')[-1] if k!='mode' else rest.split('}, mode')[-1].replace(' ',' ',1)); 
        tail=rest.split('}, mode')[-1]
        d['verdict']=re.search(r'verdict: "([^"]*)"',tail).group(1)
        d['decision']=re.search(r'decision: (\w+)',tail).group(1)
        d['available']=re.search(r'available: (\w+)',tail).group(1)=='true'
        d['hv']=re.search(r'harness_version: "([^"]*)"',tail).group(1)
        d['at']=int(re.search(r'at: (\d+)',tail).group(1))
        d['t']=datetime.datetime.fromtimestamp(d['at']).strftime('%m-%d %H:%M:%S')
    if kind=='Write':
        d['with_deps']='with_deps: true' in rest; d['epoch']=int(re.search(r'Epoch\((\d+)\)',rest).group(1))
    if kind=='Drift':
        for k in ['shell_actions','writes','writes_with_deps']:
            d[k]=int(re.search(k+r': (\d+)',rest).group(1))
        d['undeclared_n']=rest.count('"',rest.find('undeclared'),rest.find('unrealised'))//2
    out.append(d)
json.dump(out,open(S+'/tel.json','w'))
g=[d for d in out if d['kind']=='Gate']
print(len(g), sum(1 for d in g if d['command'] is None and d['tool']=='Shell'))
