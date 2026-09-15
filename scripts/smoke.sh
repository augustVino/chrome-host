#!/usr/bin/env bash
# 冒烟：环境 CRUD + hosts 注入 + 实例生命周期 + Agent API 端点 + 登录态（launch/capture/reset/409/克隆）
# 前置：应用已运行（Agent API on 127.0.0.1:17890）
# 可选环境变量 HOSTS_SOURCE：返回 hosts 格式文本的 http(s) URL；未提供时跳过 hosts 注入相关断言
set -euo pipefail

HOSTS_SOURCE="${HOSTS_SOURCE:-}"

BASE="http://127.0.0.1:17890/api/v1"
PASS=0; FAIL=0

ok()   { PASS=$((PASS+1)); echo "  ✓ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
check(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3（期望 $2 实际 $1）"; fi }
jqpy() { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

command -v python3 >/dev/null || { echo "需要 python3"; exit 1; }
curl -s --max-time 3 "$BASE/environments" >/dev/null || { echo "Agent API 未就绪 ($BASE)"; exit 1; }

echo "[0/10] 内核状态 + MCP"
KS=$(curl -s "$BASE/kernel/status" | jqpy "d['pinnedVersion']")
[ -n "$KS" ] && ok "kernel/status: pinned=$KS" || bad "kernel/status 无响应"
# MCP over Streamable HTTP：initialize 握手（响应为 SSE 帧，提取 data 行）
MCP_NAME=$(curl -s -X POST http://127.0.0.1:17890/mcp -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' | python3 -c "
import sys, json
for line in sys.stdin.read().splitlines():
    if line.startswith('data:'):
        data = line[5:].strip()
        if data:
            print(json.loads(data)['result']['serverInfo']['name'])
            break
")
check "$MCP_NAME" "chrome-host" "MCP initialize 握手"

echo "[1/10] 创建环境"
if [ -n "$HOSTS_SOURCE" ]; then
  ENV=$(curl -s -X POST "$BASE/environments" -H 'content-type: application/json' \
    -d "{\"name\":\"smoke-$(date +%s)\",\"hostsSourceUrl\":\"$HOSTS_SOURCE\"}")
else
  ENV=$(curl -s -X POST "$BASE/environments" -H 'content-type: application/json' \
    -d "{\"name\":\"smoke-$(date +%s)\"}")
fi
ENV_ID=$(echo "$ENV" | jqpy "d['id']")
[ -n "$ENV_ID" ] && ok "环境已创建 $ENV_ID" || bad "环境创建失败: $ENV"

echo "[2/10] PATCH 环境（改名 + 清空 hosts 源 + 再设置）"
PATCHED=$(curl -s -X PATCH "$BASE/environments/$ENV_ID" -H 'content-type: application/json' \
  -d '{"name":"smoke-renamed","hostsSourceUrl":null}')
NAME=$(echo "$PATCHED" | jqpy "d['name']")
check "$NAME" "smoke-renamed" "PATCH 改名生效"
HS=$(echo "$PATCHED" | jqpy "d['hostsSourceUrl']")
check "$HS" "None" "PATCH 置空 hostsSourceUrl"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PATCH "$BASE/environments/$ENV_ID" -H 'content-type: application/json' \
  -d '{"hostsSourceUrl":"notaurl"}')
check "$CODE" "400" "非法 hostsSourceUrl → 400 HOSTS_SOURCE_INVALID"
if [ -n "$HOSTS_SOURCE" ]; then
curl -s -X PATCH "$BASE/environments/$ENV_ID" -H 'content-type: application/json' \
  -d "{\"hostsSourceUrl\":\"$HOSTS_SOURCE\"}" > /dev/null
fi

echo "[2.5/10] Extensions"
EXT_DIR="${TMPDIR:-/tmp}/smoke-ext-$(date +%s)"
mkdir -p "${EXT_DIR}"
printf '{"name":"Smoke Marker","version":"0.1.0","manifest_version":3}' > "${EXT_DIR}/manifest.json"
EXT=$(curl -s -X POST "$BASE/extensions" -H 'content-type: application/json' -d "{\"path\":\"${EXT_DIR}\"}")
EXT_ID=$(echo "$EXT" | jqpy "d['id']")
[ -n "${EXT_ID}" ] && ok "注册用户扩展（manifest 元数据提取）" || bad "扩展注册失败: ${EXT}"
NSYS=$(curl -s "$BASE/extensions" | jqpy "len([e for e in d if e['type']=='system'])")
check "$NSYS" "1" "内置 system 扩展种子（env-label）"

# 按任意路径注册非法目录应 400（服务端校验 manifest）
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/extensions" -H 'content-type: application/json' -d '{"path":"/nonexistent/ext-dir"}')
check "$CODE" "400" "非法路径注册 → 400"

echo "[3/10] 创建 Instance（hosts 现拉注入）"
INS=$(curl -s --max-time 900 -X POST "$BASE/environments/$ENV_ID/instances")
INS_ID=$(echo "$INS" | jqpy "d.get('id','')")
HR=$(echo "$INS" | jqpy "len(json.loads(d['hostRules'])) if d.get('hostRules') else 0")
[ -n "$INS_ID" ] && ok "Instance 已创建 $INS_ID" || bad "Instance 创建失败: $INS"
if [ -n "$HOSTS_SOURCE" ]; then
  [ "$HR" -gt 0 ] && ok "hosts 快照已落库（$HR 条映射）" || bad "hostRules 快照为空"
else
  check "$HR" "0" "未配置 hosts 源 → 无注入"
fi

echo "[4/10] 状态/cdp/focus"
STATUS=$(curl -s "$BASE/instances/$INS_ID/status" | jqpy "d['status']")
check "$STATUS" "running" "实例运行中"
WS=$(curl -s "$BASE/instances/$INS_ID/cdp" | jqpy "1 if d.get('webSocketUrl') else 0")
check "$WS" "1" "CDP endpoint 含 webSocketUrl"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/instances/$INS_ID/focus")
check "$CODE" "200" "focus 唤出"

echo "[4.5/10] Tab API + navigate"
NTAB=$(curl -s -X POST "$BASE/instances/$INS_ID/tabs" -H 'content-type: application/json' -d '{"url":"https://example.com"}')
NTID=$(echo "$NTAB" | jqpy "d['id']")
[ -n "$NTID" ] && ok "POST tabs 新建标签页" || bad "POST tabs 失败: $NTAB"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/instances/$INS_ID/tabs" -H 'content-type: application/json' -d '{"url":"notaurl"}')
check "$CODE" "400" "非法 URL → 400"
curl -s -X POST "$BASE/instances/$INS_ID/navigate" -H 'content-type: application/json' -d "{\"tabId\":\"$NTID\",\"url\":\"https://example.org\"}" > /dev/null
CODE=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/instances/$INS_ID/tabs")
check "$CODE" "200" "GET tabs 可用"
LOAD=$(ps -p $(curl -s "$BASE/instances/$INS_ID" | jqpy "d['pid']") -o command= | grep -c "load-extension" || true)
check "$LOAD" "1" "实例加载扩展（--load-extension）"
CMD=$(ps -p $(curl -s "$BASE/instances/$INS_ID" | jqpy "d['pid']") -o command=)
echo "${CMD}" | grep -q -- "--load-extension" && echo "${CMD}" | grep -q "${EXT_DIR}" && ok "实例加载 user 扩展（--load-extension 含注册路径）" || bad "--load-extension 未含 user 扩展路径"

echo "[5/10] 环境运行时摘要"
SUMMARY=$(curl -s "$BASE/environments/$ENV_ID/status" | jqpy "d['instances']['running']")
check "$SUMMARY" "1" "env status: 1 running"

echo "[6/10] restart（stop+start，hosts 重拉）"
STATUS=$(curl -s --max-time 120 -X POST "$BASE/instances/$INS_ID/restart" | jqpy "d['status']")
check "$STATUS" "running" "restart 后仍 running"

echo "[7/10] 重复 start 应 409"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/instances/$INS_ID/start")
check "$CODE" "409" "409 INSTANCE_ALREADY_RUNNING"

echo "[8/10] stop-all（环境级）"
STOPPED=$(curl -s -X POST "$BASE/environments/$ENV_ID/stop-all" | jqpy "len(d['stopped'])")
check "$STOPPED" "1" "stop-all 停止 1 个实例"
STATUS=$(curl -s "$BASE/instances/$INS_ID/status" | jqpy "d['status']")
check "$STATUS" "stopped" "实例已停止"

echo "[8.5/10] Settings + Activity"
D0=$(curl -s "$BASE/settings" | jqpy "1 if d['developerMode'] else 0")
curl -s -X PUT "$BASE/settings" -H 'content-type: application/json' -d '{"developerMode":true}' > /dev/null
D1=$(curl -s "$BASE/settings" | jqpy "1 if d['developerMode'] else 0")
curl -s -X PUT "$BASE/settings" -H 'content-type: application/json' -d '{"developerMode":false}' > /dev/null
[ "$D0" = "0" ] && [ "$D1" = "1" ] && ok "Developer Mode 开关持久化" || bad "Developer Mode 开关异常 (${D0}→${D1})"
curl -s -X PATCH "$BASE/environments/$ENV_ID" -H 'content-type: application/json' -d '{"keepAlive":true}' > /dev/null
KA=$(curl -s "$BASE/environments/$ENV_ID" | jqpy "1 if d['keepAlive'] else 0")
check "$KA" "1" "PATCH keepAlive 生效"
ACT=$(curl -s "$BASE/environments/$ENV_ID/activity?limit=10" | jqpy "len(d)")
[ "$ACT" -ge 1 ] && ok "Activity 流有事件（$ACT 条）" || bad "Activity 流为空"

echo "[9/10] Login Profile：惰性建行 → capture → reset → launch → 409"
LP=$(curl -s "$BASE/environments/$ENV_ID/login-profile")
ST=$(echo "$LP" | jqpy "d['status']")
check "$ST" "not_configured" "login-profile 惰性建行"
LP=$(curl -s --max-time 30 -X POST "$BASE/environments/$ENV_ID/login-profile/capture")
ST=$(echo "$LP" | jqpy "d['status']")
V=$(echo "$LP" | jqpy "d['snapshotVersion']")
check "$ST" "ready" "capture 空母本 → ready"
check "$V" "1" "snapshotVersion 前进到 1"
LP=$(curl -s -X POST "$BASE/environments/$ENV_ID/login-profile/reset")
ST=$(echo "$LP" | jqpy "d['status']")
V=$(echo "$LP" | jqpy "d['snapshotVersion']")
check "$ST,$V" "not_configured,0" "reset 归零回 not_configured"
CODE=$(curl -s --max-time 90 -o /dev/null -w "%{http_code}" -X POST "$BASE/environments/$ENV_ID/login-profile/launch")
check "$CODE" "200" "launch 登录浏览器"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/environments/$ENV_ID/login-profile/launch")
check "$CODE" "409" "重复 launch → 409 PROFILE_IN_USE"
RUNNING=$(curl -s "$BASE/environments/$ENV_ID/login-profile" | jqpy "1 if d['browserRunning'] else 0")
check "$RUNNING" "1" "browserRunning=true"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$BASE/environments/$ENV_ID/login-profile/capture")
check "$CODE" "409" "运行中 capture → 409（不代杀）"
# 模拟 macOS 关窗：CDP 关闭全部页面，进程不退 → 服务应双确认后代为收尾
LP_CDP=$(curl -s "$BASE/environments/$ENV_ID/login-profile" | jqpy "d['cdpPort']")
curl -s "http://127.0.0.1:$LP_CDP/json/list" | python3 -c "import sys,json;[print(t['id']) for t in json.load(sys.stdin) if t['type']=='page']" | while read -r tid; do
  curl -s "http://127.0.0.1:$LP_CDP/json/close/$tid" > /dev/null
done
RUNNING=1
for i in $(seq 1 25); do
  RUNNING=$(curl -s "$BASE/environments/$ENV_ID/login-profile" | jqpy "1 if d['browserRunning'] else 0")
  [ "$RUNNING" = "0" ] && break
  sleep 2
done
check "$RUNNING" "0" "关窗后 browserRunning 回正并代收进程"
# 进程死亡路径：重新 launch 后 pkill 主进程 → watcher 事件回正
CODE=$(curl -s --max-time 90 -o /dev/null -w "%{http_code}" -X POST "$BASE/environments/$ENV_ID/login-profile/launch")
check "$CODE" "200" "关窗后可重新 launch"
pkill -TERM -f "com.chromehost.dev/environments/.*login-profile" || true
for i in $(seq 1 15); do
  RUNNING=$(curl -s "$BASE/environments/$ENV_ID/login-profile" | jqpy "1 if d['browserRunning'] else 0")
  [ "$RUNNING" = "0" ] && break
  sleep 0.5
done
check "$RUNNING" "0" "进程退出后 browserRunning 回正"
LP=$(curl -s --max-time 30 -X POST "$BASE/environments/$ENV_ID/login-profile/capture")
ST=$(echo "$LP" | jqpy "d['status']")
check "$ST" "ready" "关窗后 capture → ready"

echo "[9.8/10] Extensions 生命周期"
EN=$(curl -s -X PATCH "$BASE/extensions/$EXT_ID" -H 'content-type: application/json' -d '{"enabled":false}' | jqpy "d['enabled']")
check "$EN" "False" "PATCH 禁用扩展"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$BASE/extensions/$EXT_ID")
check "$CODE" "200" "移除扩展注册项"
CODE=$(curl -s -o /dev/null -w "%{http_code}" "$BASE/extensions/$EXT_ID")
check "$CODE" "404" "移除后不可见"

echo "[10/10] start（已停止实例重启）+ 快照克隆 + 删除"
STATUS=$(curl -s --max-time 120 -X POST "$BASE/instances/$INS_ID/start" | jqpy "d['status']")
check "$STATUS" "running" "start 重启成功"
curl -s -X POST "$BASE/instances/$INS_ID/stop" > /dev/null
INS2=$(curl -s --max-time 120 -X POST "$BASE/environments/$ENV_ID/instances")
INS2_ID=$(echo "$INS2" | jqpy "d.get('id','')")
LPID=$(echo "$INS2" | jqpy "d.get('loginProfileId','')")
[ -n "$LPID" ] && ok "快照 ready 后新建实例克隆登录态（loginProfileId=${LPID}）" || bad "新实例未克隆快照"
curl -s -X POST "$BASE/instances/$INS2_ID/stop" > /dev/null
curl -s -X DELETE "$BASE/instances/$INS2_ID" > /dev/null
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$BASE/instances/$INS_ID")
check "$CODE" "200" "实例已删除"
CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$BASE/environments/$ENV_ID")
check "$CODE" "200" "环境已删除（含 login profile 级联）"

echo
echo "结果: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ]
