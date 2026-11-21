#!/usr/bin/env python3
import json, os, sys
from urllib.parse import urlsplit, parse_qs
p = os.environ['FIXTURE']
s = json.load(open(p))
a = sys.argv[1:]
with open(p + '.calls', 'a') as f: f.write(json.dumps(a) + '\n')
if s.get('stall_at') and (s['stall_at'] == 'diff' and a[:2] == ['pr', 'diff'] or s['stall_at'] in a):
    import time
    with open(p + '.stalled', 'w') as f: f.write(str(os.getpid()))
    time.sleep(60)
if a[:2] == ['repo', 'clone']:
    import subprocess
    sys.exit(subprocess.call(['git','clone',s['remote'],a[3]]))
if a[:2] == ['pr', 'diff']:
    if s.get('fail_diff'):
        print('diff unavailable', file=sys.stderr)
        sys.exit(1)
    if int(a[2]) in s.get('github_oversized_prs', []):
        print('could not find pull request diff: HTTP 406: Sorry, the diff exceeded the maximum number of lines (20000)\nPullRequest.diff too_large', file=sys.stderr)
        sys.exit(1)
    if a[2] in s.get('diffs', {}):
        print(s['diffs'][a[2]])
        sys.exit(0)
    if int(a[2]) in s.get('oversized_prs', []):
        print('x' * (512 * 1024 + 1))
        sys.exit(0)
    print('diff --git a/db.rs b/db.rs\n--- a/db.rs\n+++ b/db.rs\n@@ -1 +1 @@\n-bug\n+fix')
    sys.exit(0)
assert a[:3] == ['api', '--method', a[2]], a
method, endpoint = a[2:4]
body = json.load(sys.stdin) if '--input' in a else None
if method != 'GET':
    with open(p + '.writes', 'a') as f:
        f.write(json.dumps(dict(method=method, endpoint=endpoint, body=body)) + '\n')
path = endpoint.split('?')[0]
root = 'repos/owner/repo'
result = None
if s.get('rate_limit_endpoint') == path:
    print('HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 4102444800\r\n\r\n{}')
    print('gh: API rate limit exceeded for user ID 1 (HTTP 403)', file=sys.stderr)
    sys.exit(1)
if path == 'user': result = {'login':'fiach-bot'}
elif path == root + '/issues':
    query = parse_qs(urlsplit(endpoint).query)
    page = int(query.get('page',['1'])[0]); size = int(query.get('per_page',['100'])[0])
    if s.get('fail_inventory_page') == page: sys.exit(1)
    result = sorted(s['items'], key=lambda i: i['number'])[(page-1)*size:page*size]
elif path == root + '/labels':
    result = [{'name':x} for x in s['labels']]
    if method == 'POST': s['labels'].append(body['name']); result = body
elif path == root + '/issues/comments':
    query = parse_qs(urlsplit(endpoint).query)
    page = int(query.get('page',['1'])[0]); size = int(query.get('per_page',['100'])[0])
    if s.get('fail_comments_page') == page: sys.exit(1)
    comments = [dict(c, issue_url='https://api.github.com/' + root + '/issues/' + n) for n, cs in s['comments'].items() for c in cs]
    result = comments[(page-1)*size:page*size]
elif path.startswith(root + '/issues/comments/') and path.endswith('/reactions'):
    assert method == 'POST' and body == {'content':'eyes'}
    comment = int(path.split('/')[-2])
    assert any(c['id'] == comment for cs in s['comments'].values() for c in cs)
    if s.get('fail_reaction'):
        print('reaction unavailable', file=sys.stderr)
        sys.exit(1)
    reactions = s.setdefault('reactions', [])
    result = {'comment':comment, 'content':'eyes', 'user':'fiach-bot'}
    if result not in reactions: reactions.append(result)
    if s.get('lose_reaction_response'):
        s['lose_reaction_response'] = False
        json.dump(s,open(p,'w'))
        sys.exit(1)
elif path.startswith(root + '/issues/comments/'):
    comment = next(c for cs in s['comments'].values() for c in cs if c['id'] == int(path.split('/')[-1]))
    if method == 'DELETE':
        for cs in s['comments'].values():
            if comment in cs: cs.remove(comment)
    else:
        assert method == 'PATCH'
        comment['body'] = body['body']; result = comment
elif path.startswith(root + '/issues/'):
    tail = path[len(root + '/issues/'):].split('/')
    n = int(tail[0]); issue = next((i for i in s['items'] if i['number'] == n), {'number':n, 'updated_at':'pr'})
    if len(tail) == 1: result = issue
    elif tail[1] == 'timeline': result = s.get('timeline', {}).get(str(n), [])
    elif tail[1] == 'events': result = s.get('events', {}).get(str(n), [])
    elif tail[1] == 'comments':
        comments = s['comments'].setdefault(str(n), [])
        if method == 'POST':
            comments.append({'id':1000+n+len(comments),'user':{'login':'fiach-bot'},'body':body['body']})
            issue['updated_at'] = 'bot-updated'
            result = comments[-1]
        else: result = comments
    elif tail[1] == 'labels':
        if method == 'POST':
            for label in body['labels']:
                if not any(l['name'] == label for l in issue['labels']): issue['labels'].append({'name':label})
        elif method == 'DELETE':
            from urllib.parse import unquote
            issue['labels'] = [l for l in issue['labels'] if l['name'] != unquote(tail[2])]
        else: raise Exception(a)
        result = issue['labels']
    else: raise Exception(a)
elif path.startswith(root + '/git/ref/heads/') or path.startswith(root + '/commits/'):
    import subprocess
    ref = path.split('/heads/',1)[1] if '/heads/' in path else path.split('/commits/',1)[1]
    sha = subprocess.check_output(['git','rev-parse',ref],cwd=s['remote'],text=True).strip()
    result = {'object':{'sha':sha},'sha':sha}
elif path == root + '/pulls':
    if method == 'POST':
        if s.get('fail_pr_create'):
            s['fail_pr_create'] = False
            json.dump(s,open(p,'w'))
            sys.exit(1)
        assert body['draft'] is True
        assert 'Fixes' not in body['body'] and 'Closes' not in body['body']
        result = dict(body, html_url='https://github.com/owner/repo/pull/%d' % (9 + len(s.get('prs', []))), state='open')
        s.setdefault('prs', []).append(result)
        if s.get('lose_pr_response'):
            s['lose_pr_response'] = False
            json.dump(s,open(p,'w'))
            sys.exit(1)
    else:
        head = parse_qs(urlsplit(endpoint).query).get('head', [':'])[0].split(':', 1)[1]
        result = [pr for pr in s.get('prs', []) if not head or pr.get('head') == head]
elif path.startswith(root + '/pulls/'):
    tail = path[len(root + '/pulls/'):].split('/')
    n = int(tail[0])
    if len(tail) > 1:
        assert tail[1] in ['reviews', 'comments']
        result = s.get('pr_' + tail[1], [])
    elif 0 <= n - 9 < len(s.get('prs', [])):
        import subprocess
        pr = s['prs'][n - 9]
        sha = lambda ref: subprocess.check_output(['git','rev-parse',ref],cwd=s['remote'],text=True).strip()
        result = {'number':n, 'title':pr['title'], 'state':pr['state'], 'merged':s.get('iteration_merged',False),
            'user':{'login':'fiach-bot'},
            'head':{'sha':sha(pr['head']), 'ref':pr['head'], 'repo':{'full_name':s.get('head_repo','owner/repo')}},
            'base':{'sha':sha(pr['base']), 'ref':pr['base'], 'repo':{'full_name':'owner/repo'}}}
    else:
        result = {'state':'open','head':{'sha':'abc'},'base':{'sha':'base'}}
        s['pull_reads'] = s.get('pull_reads', 0) + 1
        if s.get('change_revision') and s['pull_reads'] >= 3:
            result[s['change_revision']]['sha'] = 'changed'
else: raise Exception(a)
json.dump(s,open(p,'w'))
if '--include' in a: print('HTTP/2.0 200 OK\r\nX-RateLimit-Remaining: 4000\r\n\r\n', end='')
print(json.dumps(result))
