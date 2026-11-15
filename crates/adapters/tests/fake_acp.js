const readline = require('readline');
const mode = process.argv[2]; let promptId, n=0;
const send = x => process.stdout.write(JSON.stringify(x)+'\n');
const update = u => send({jsonrpc:'2.0',method:'session/update',params:{sessionId:'s',update:u}});
readline.createInterface({input:process.stdin}).on('line', line => {
 const v=JSON.parse(line);
 if(v.method==='initialize') { if(mode==='hang') return; send({jsonrpc:'2.0',id:v.id,result:{protocolVersion:1,authMethods:[{id:'login',name:'login'}]}}); }
 else if(v.method==='session/new') { if(mode==='newhang') return; if(mode==='auth') send({id:v.id,error:{code:-32000,message:'Authentication required'}}); else send({id:v.id,result:{sessionId:'s'}}); }
 else if(v.method==='session/prompt') { promptId=v.id; update({sessionUpdate:'agent_thought_chunk',content:{type:'text',text:'think'}}); update({sessionUpdate:'agent_message_chunk',content:{type:'text',text:'hello'}}); update({sessionUpdate:'tool_call',toolCallId:'t',status:'pending',title:'write',kind:'edit'}); update({sessionUpdate:'tool_call_update',toolCallId:'t',rawInput:{filePath:'a.txt',content:'hi'},status:'in_progress'}); send({id:101,method:'session/request_permission',params:{toolCall:{kind:'execute',rawInput:{command:'git status'}},options:[{optionId:'yes',kind:'allow_once'},{optionId:'no',kind:'reject_once'}]}}); }
 else if(v.id===101) { if(v.result.outcome.optionId!=='yes') process.exit(2); send({id:102,method:'session/request_permission',params:{toolCall:{kind:'execute',rawInput:{command:'git status'}},options:[{optionId:'yes',kind:'allow_once'},{optionId:'no',kind:'reject_once'}]}}); }
 else if(v.id===102) { if(v.result.outcome.optionId!=='no') process.exit(3); update({sessionUpdate:'tool_call_update',toolCallId:'t',status:'completed'}); }
 else if(v.method==='session/cancel') send({id:promptId,result:{stopReason:'cancelled'}});
});

if (mode === "orphan") process.stdin.on("end", () => setInterval(() => {}, 1000));
