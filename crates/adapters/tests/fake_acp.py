import sys,json
mode=sys.argv[1]; prompt_id=None
def send(x): print(json.dumps(x),flush=True)
def update(u): send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'s','update':u}})
for line in sys.stdin:
 v=json.loads(line); method=v.get('method')
 if method=='initialize':
  if mode!='hang': send({'id':v['id'],'result':{'protocolVersion':1,'authMethods':[{'id':'login','name':'login'}]}})
 elif method=='session/new':
  if mode=='newhang': continue
  send({'id':v['id'],'error':{'code':-32000,'message':'Authentication required'}} if mode=='auth' else {'id':v['id'],'result':{'sessionId':'s'}})
 elif method=='session/prompt':
  prompt_id=v['id']; update({'sessionUpdate':'agent_thought_chunk','content':{'type':'text','text':'think'}}); update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'hello'}}); update({'sessionUpdate':'tool_call','toolCallId':'t','status':'pending','title':'write','kind':'edit'}); update({'sessionUpdate':'tool_call_update','toolCallId':'t','rawInput':{'filePath':'a.txt','content':'hi'},'status':'in_progress'}); send({'id':101,'method':'session/request_permission','params':{'toolCall':{'kind':'execute','rawInput':{'command':'git status'}},'options':[{'optionId':'yes','kind':'allow_once'},{'optionId':'no','kind':'reject_once'}]}})
 elif v.get('id')==101:
  if v['result']['outcome']['optionId']!='yes': sys.exit(2)
  send({'id':102,'method':'session/request_permission','params':{'toolCall':{'kind':'execute','rawInput':{'command':'git status'}},'options':[{'optionId':'yes','kind':'allow_once'},{'optionId':'no','kind':'reject_once'}]}})
 elif v.get('id')==102:
  if v['result']['outcome']['optionId']!='no': sys.exit(3)
  update({'sessionUpdate':'tool_call_update','toolCallId':'t','status':'completed'})
 elif method=='session/cancel': send({'id':prompt_id,'result':{'stopReason':'cancelled'}})

if mode == "orphan":
 import time
 while True: time.sleep(1)
