# Deterministic sanitized ACP sequence. No live model or historical journal IO.
import json
import os
import sys

scenario = os.environ['SCENARIO']
artifact = os.path.join(os.environ['CLAUDE_CONFIG_DIR'], 'plans', 'proposal.md')
with open(os.environ['FIXTURE'], encoding='utf-8') as source:
    template = json.load(source)
mode = [{'id': 'mode', 'name': 'Mode', 'category': 'mode', 'type': 'select', 'currentValue': 'read-only', 'options': [{'value': 'read-only', 'name': 'Plan'}]}]
generation = 0
prompt_count = 0
pending_prompt = None
refining_edit = scenario in ['edit-refinement', 'timeout-edit-refinement', 'edit-timeout']
with open(os.environ['TRACE'] + '.processes', 'a', encoding='utf-8', newline='\n') as trace:
    trace.write('spawn\n')

def send(value):
    print(json.dumps(value), flush=True)

def update(value):
    send({'jsonrpc': '2.0', 'method': 'session/update', 'params': {'sessionId': 'native-session', 'update': value}})

def result(request, value):
    send({'jsonrpc': '2.0', 'id': request, 'result': value})

for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    request = message.get('id')
    if method == 'initialize':
        result(request, {'protocolVersion': 1, 'agentCapabilities': {}, 'agentInfo': {'name': 'native-fixture', 'version': '1'}})
    elif method == 'session/new':
        result(request, {'sessionId': 'native-session', 'configOptions': mode})
    elif method == 'session/set_config_option':
        result(request, {'configOptions': mode})
    elif method == 'session/prompt':
        prompt_count += 1
        assert message['params']['sessionId'] == 'native-session', message
        if scenario == 'timeout-edit-refinement':
            if prompt_count == 1:
                send({'jsonrpc': '2.0', 'id': request, 'error': template['timeout_error']})
                continue
            assert prompt_count == 2, message
            assert message['params']['prompt'] == [{'type': 'text', 'text': 'continue'}], message
        # A transient continuation is not a new logical proposal generation.
        generation += 1
        if scenario == 'retry' and generation > 1:
            update(fixture['exit_terminal'])  # late old-generation terminal replay
        pending_prompt = request
        fixture = expand_fixture_template(template, artifact, generation)
        missing = scenario == 'retry' and generation != 2
        if refining_edit:
            fixture['write']['rawInput']['content'] = fixture['edit_initial_markdown']
        if not missing:
            update(fixture['write'])
            # Native Write executes without an ACP permission, as in the journal.
            with open(artifact, 'w', encoding='utf-8', newline='\n') as output:
                output.write(fixture['write']['rawInput']['content'])
        if refining_edit:
            update(fixture['write_terminal'])
            update(fixture['edit_announcement'])
            update(fixture['edit_path'])
            update(fixture['edit_preview'])
            preview = fixture['edit_preview']['content'][0]
            expanded = fixture['edit_result']['content'][0]
            before = fixture['edit_initial_markdown']
            after = before.replace(preview['oldText'], preview['newText'], 1)
            assert after == before.replace(expanded['oldText'], expanded['newText'], 1)
            assert after == fixture['edited_markdown']
            with open(artifact, 'w', encoding='utf-8', newline='\n') as output:
                output.write(after)
            update(fixture['edit_result'])
            if scenario != 'edit-timeout':
                update(fixture['edit_terminal'])
        if scenario == 'explicit' or (scenario == 'retry' and generation == 2):
            # Deliberately differs from the file: explicit payload is authoritative.
            for tool in [fixture['exit'], fixture['permission']['toolCall']]:
                tool['rawInput'] = {'plan': '# Explicit proposal\n\n* Preserve Plan mode.'}
        update(fixture['exit'])
        send({'jsonrpc': '2.0', 'id': 'exit-permission', 'method': 'session/request_permission', 'params': fixture['permission']})
    elif request == 'exit-permission' and 'result' in message:
        assert message['result']['outcome'] == {'outcome': 'selected', 'optionId': 'stay-planning-exact-id'}, message
        with open(os.environ['TRACE'], 'a', encoding='utf-8', newline='\n') as trace:
            trace.write('reject_once\n')
        # A host waiting inside the permission callback deadlocks here: the
        # terminal is never emitted until the exact rejection response arrives.
        update(fixture['exit_terminal'])
        if refining_edit:
            # Remain live until bounded host cancellation. For edit-timeout,
            # successful diff output and an on-disk file cannot replace the
            # deliberately missing Edit terminal evidence.
            continue
        if scenario in ['prompt-first', 'timeout', 'retry']:
            result(pending_prompt, {'stopReason': 'end_turn'})
            pending_prompt = None
        if scenario in ['transient', 'transient-timeout']:
            send({'jsonrpc': '2.0', 'id': pending_prompt, 'error': {'code': -32603, 'message': 'API Error: Connection dropped (ECONNRESET)', 'data': {'errorKind': 'server_error'}}})
            pending_prompt = None
        if scenario == 'disconnect':
            sys.exit(17)
        if scenario not in ['timeout', 'transient-timeout'] and not (scenario == 'retry' and generation != 2):
            update(fixture['write_terminal'])
        if scenario not in ['prompt-first', 'timeout', 'retry', 'capture-first', 'transient', 'transient-timeout']:
            result(pending_prompt, {'stopReason': 'end_turn'})
            pending_prompt = None
    elif method == 'session/cancel' and pending_prompt is not None:
        result(pending_prompt, {'stopReason': 'cancelled'})
        pending_prompt = None
