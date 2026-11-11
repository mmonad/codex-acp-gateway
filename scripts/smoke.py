#!/usr/bin/env python3
"""Validate the gateway against schemas emitted by the installed Codex binary.
Run with: uv run --with jsonschema scripts/smoke.py [--live]
"""
import argparse,json,os,pathlib,queue,signal,subprocess,tempfile,threading,time
import jsonschema
parser=argparse.ArgumentParser();parser.add_argument('--live',action='store_true');parser.add_argument('--live-model',action='append');args=parser.parse_args()
root=pathlib.Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='acp-smoke-') as tmp:
 work=pathlib.Path(tmp);schemas=work/'schemas'
 subprocess.run(['codex','app-server','generate-json-schema','--out',str(schemas)],check=True)
 (work/'fixture.txt').write_text('ACP_FIXTURE_72914\n')
 env=os.environ.copy();env['ACP_ALLOWED_MODELS']='gemini-4-carbon,gemini-4-argon,gemini-3.8-flash';env['ACP_GATEWAY_HOME']=str(work/'data')
 log=(work/'gateway.log').open('w')
 process=subprocess.Popen([str(root/'target/release/codex-acp-gateway'),'--agent-cmd',str(pathlib.Path.home()/'.local/bin/agy_acp_server.par'),'--cwd',str(work)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True,env=env,start_new_session=True)
 messages=queue.Queue()
 def reader(proc):
  for line in proc.stdout:
   try:messages.put(json.loads(line))
   except ValueError:print('non-json:',line)
 threading.Thread(target=reader,args=(process,),daemon=True).start()
 counter=0;events=[]
 def send(data):process.stdin.write(json.dumps(data)+'\n');process.stdin.flush()
 def receive(timeout=60):
  message=messages.get(timeout=timeout)
  if 'method' in message:
   if not message['method'].startswith('gateway/') and 'id' not in message:
    schema=json.loads((schemas/'ServerNotification.json').read_text())
    errors=list(jsonschema.Draft7Validator(schema).iter_errors(message))
    if errors:
     raise AssertionError('Invalid notification '+message['method']+': '+str(message)[:1500]+' '+str(errors[0].message)[:100])
   events.append(message)
   if 'id' in message:
    if message['method'].endswith('/requestApproval'):send({'id':message['id'],'result':{'decision':'accept'}})
    else:send({'id':message['id'],'error':{'code':-32601,'message':'Unsupported smoke request'}})
  return message
 def request(method,params,schema=None):
  global counter
  counter+=1;ident=counter;send({'id':ident,'method':method,'params':params})
  while True:
   m=receive()
   if m.get('id')==ident and 'method' not in m:
    if 'error' in m:raise RuntimeError(m['error'])
    if schema:
     paths=list(schemas.rglob(schema+'.json'));jsonschema.validate(m['result'],json.loads(paths[0].read_text()))
    return m['result']
 try:
  request('initialize',{'clientInfo':{'name':'acp-smoke','version':'1.0'},'capabilities':{'experimentalApi':True}},'InitializeResponse')
  request('config/read',{},'ConfigReadResponse')
  models=request('model/list',{},'ModelListResponse')['data']
  assert {m['id'] for m in models}==set(env['ACP_ALLOWED_MODELS'].split(',')),models
  for m in models:assert [e['reasoningEffort'] for e in m['supportedReasoningEfforts']]==['low','medium','high'],m
  print('PASS current Codex schemas; exactly three models, three efforts each',flush=True)
  for m in models:
   for effort in ['low','medium','high']:
    result=request('thread/start',{'cwd':str(work),'model':m['id'],'config':{'model_reasoning_effort':effort}},'ThreadStartResponse')
    assert result['model']==m['id'] and result['reasoningEffort']==effort,result
   print('PASS selection:',m['id'],'low/medium/high',flush=True)
  request('config/value/write',{'keyPath':'model','value':'gemini-3.8-flash','mergeStrategy':'replace'},'ConfigWriteResponse')
  assert request('config/read',{},'ConfigReadResponse')['config']['model']=='gemini-3.8-flash'
  thread=result['thread']['id']
  command=process.args
  os.killpg(process.pid,signal.SIGTERM);process.wait(timeout=5)
  process=subprocess.Popen(command,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True,env=env,start_new_session=True)
  messages=queue.Queue()
  threading.Thread(target=reader,args=(process,),daemon=True).start()
  request('initialize',{'clientInfo':{'name':'acp-smoke','version':'1.0'}},'InitializeResponse')
  resumed=request('thread/resume',{'threadId':thread,'cwd':str(work),'model':'gemini-3.8-flash'},'ThreadResumeResponse')
  assert resumed['model']=='gemini-3.8-flash'
  request('thread/settings/update',{'threadId':thread,'model':'gemini-4-carbon','effort':'medium'})
  check=request('thread/resume',{'threadId':thread,'cwd':str(work)},'ThreadResumeResponse')
  assert check['model']=='gemini-4-carbon' and check['reasoningEffort']=='medium',check
  print('PASS saved settings, restart/resume and model-picker updates',flush=True)
  if args.live:
   for model in (args.live_model or ['gemini-3.8-flash','gemini-4-argon','gemini-4-carbon']):
    start=len(events)
    prompt='Reply with exactly ACP_OK. Do not use tools.'
    if model=='gemini-3.8-flash':prompt='Use a file reading tool to read fixture.txt in the current directory. Reply with only its contents.'
    request('turn/start',{'threadId':thread,'model':model,'effort':'low','input':[{'type':'text','text':prompt}]},'TurnStartResponse')
    deadline=time.monotonic()+120
    while time.monotonic()<deadline:
     event=receive(timeout=120)
     if event.get('method')=='turn/completed':
      assert event['params']['turn']['status']=='completed',event
      break
    else:raise TimeoutError(model)
    turn_events=events[start:];answer=''.join(e['params'].get('delta','') for e in turn_events if e.get('method')=='item/agentMessage/delta')
    expected='ACP_FIXTURE_72914' if model=='gemini-3.8-flash' else 'ACP_OK'
    assert expected in answer,(model,answer)
    if model=='gemini-3.8-flash':
     assert any(e.get('method')=='item/started' and e['params'].get('item',{}).get('type')!='agentMessage' for e in turn_events),'No actual tool event'
    completed=[e for e in turn_events if e.get('method')=='item/completed' and e['params']['item']['type']=='agentMessage']
    assert completed and expected in completed[-1]['params']['item']['text'],completed
    print('PASS live turn:',model,repr(answer),flush=True)
 finally:
  os.killpg(process.pid, signal.SIGTERM)
  try:process.wait(timeout=5)
  except subprocess.TimeoutExpired:process.kill();process.wait()
  time.sleep(.2)
  log.close()
  if process.returncode not in [0,-15]:print((work/'gateway.log').read_text()[-4000:])
