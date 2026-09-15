#!/usr/bin/env bash
# 冒烟（CLI 门面）：chrome-host CLI 全命令组 + 三态输出 + exit code 契约
# 版本前提：chrome-host 桌面应用【新版】已运行（Agent API on 127.0.0.1:17890，且含
#   GET /api/v1/instances 与 GET /api/v1/health 端点）。
#   旧版 GUI（无这两个端点）会在前置检查被拒绝，与 smoke.sh 的「应用已运行」前提对齐。
# 长操作提示：instance create 为创建即启动，首次运行可能阻塞于内核下载（~150MB，无总超时）。
# 覆盖：CLI_BIN（如 CLI_BIN=target/debug/chrome-host bash scripts/smoke-cli.sh）
set -euo pipefail

CLI_BIN="${CLI_BIN:-cargo run -q -p chrome-host-cli --}"
API_URL="${CHROME_HOST_API_URL:-http://127.0.0.1:17890}/api/v1"
PASS=0; FAIL=0

ok()   { PASS=$((PASS+1)); echo "  ✓ $1"; }
bad()  { FAIL=$((FAIL+1)); echo "  ✗ $1"; }
check(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3（期望 $2 实际 $1）"; fi }
jqpy() { python3 -c "import sys,json;d=json.load(sys.stdin);print($1)"; }

# CLI_BIN 允许多词（cargo run … --），拆成数组后整体引用展开
# shellcheck disable=SC2206
CLI_CMD=($CLI_BIN)
cli() { "${CLI_CMD[@]}" "$@"; }

command -v python3 >/dev/null || { echo "需要 python3"; exit 1; }
curl -s --max-time 3 "$API_URL/kernel/status" >/dev/null 2>&1 || { echo "Agent API 未就绪 ($API_URL)"; exit 1; }
CODE=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 "$API_URL/instances")
[ "$CODE" = "200" ] || { echo "需要新版 GUI（缺 GET /api/v1/instances，当前 HTTP ${CODE}）"; exit 1; }

echo "[0/12] --version / --help / 互斥参数"
cli --version >/dev/null 2>&1 && ok "--version 可用" || bad "--version 失败"
HELP=$(cli --help 2>/dev/null)
MISSING=""
for name in env instance extension runtime status doctor; do
  echo "$HELP" | grep -qw "$name" || MISSING="$MISSING $name"
done
[ -z "$MISSING" ] && ok "--help 含六个命令组名" || bad "--help 缺命令组:$MISSING"
set +e; cli env list --json --quiet >/dev/null 2>&1; code=$?; set -e
check "$code" "2" "--json --quiet 互斥 → exit 2"

echo "[1/12] env create --quiet 捕获 id + env list --json"
ENV_NAME="smoke-cli-$(date +%s)"
ENV_ID=$(cli env create "$ENV_NAME" --quiet 2>/dev/null)
[ -n "$ENV_ID" ] && ok "env create --quiet 捕获 id $ENV_ID" || bad "env create 失败"
HAS=$(cli env list --json | jqpy "1 if any(e['id']=='$ENV_ID' for e in d) else 0")
check "$HAS" "1" "env list --json 含新建环境"

echo "[2/12] env update：改名 / 置空 hosts 源 / 非法 URL"
cli env update "$ENV_ID" --name smoke-cli-renamed >/dev/null 2>&1
NEW_NAME=$(cli env get "$ENV_ID" --json | jqpy "d['name']")
check "$NEW_NAME" "smoke-cli-renamed" "--name 改名生效"
NULL_URL=$(cli env update "$ENV_ID" --hosts-source-url "" --json | jqpy "1 if d['hostsSourceUrl'] is None else 0")
check "$NULL_URL" "1" "--hosts-source-url 置空 → json null"
set +e; cli env update "$ENV_ID" --hosts-source-url notaurl >/dev/null 2>&1; code=$?; set -e
check "$code" "7" "非法 hosts 源 URL（notaurl）→ exit 7"

echo "[3/12] instance create --quiet 捕获 id（长操作：可能阻塞内核下载）"
set +e; INS_ID=$(cli instance create "$ENV_ID" --quiet 2>/dev/null); code=$?; set -e
check "$code" "0" "instance create --quiet → exit 0"
if [ -z "$INS_ID" ]; then
  bad "instance create 未产出 id，提前收尾（清理环境 ${ENV_ID}）"
  cli env delete "$ENV_ID" --yes >/dev/null 2>&1 || true
  echo; echo "结果: PASS=$PASS FAIL=$FAIL"; exit 1
fi
ok "instance 捕获 id $INS_ID"
ST=$(cli instance get "$INS_ID" --json | jqpy "d['status']")
check "$ST" "running" "instance get: status=running（创建即启动）"

echo "[4/12] instance list（全局端点 + --env 过滤）"
HAS=$(cli instance list --json | jqpy "1 if any(i['id']=='$INS_ID' for i in d) else 0")
check "$HAS" "1" "全局 instance list --json 含该实例"
ENVNAME=$(cli instance list --json | jqpy "next((1 for i in d if i['id']=='$INS_ID' and i.get('environmentName')), 0)")
check "$ENVNAME" "1" "InstanceView 含 environmentName"
BYENV=$(cli instance list --env "$ENV_ID" --json | jqpy "1 if any(i['id']=='$INS_ID' for i in d) else 0")
check "$BYENV" "1" "instance list --env 过滤含该实例"

echo "[5/12] instance open（URL 本地校验）+ instance cdp"
set +e; cli instance open "$INS_ID" "notaurl" >/dev/null 2>&1; code=$?; set -e
check "$code" "2" "open 无协议 URL → exit 2"
set +e; cli instance open "$INS_ID" "https://example.com" >/dev/null 2>&1; code=$?; set -e
check "$code" "0" "open 带协议 URL → exit 0"
PORT=$(cli instance cdp "$INS_ID" --json | jqpy "1 if d.get('port') else 0")
check "$PORT" "1" "cdp --json 含 port 字段"

echo "[6/12] 409 编排：running 实例 delete --yes（stop→delete）+ 404 语义"
set +e; cli instance delete "$INS_ID" --yes >/dev/null 2>&1; code=$?; set -e
check "$code" "0" "对 running 实例 delete --yes（编排消费 409）→ exit 0"
set +e; cli instance get "$INS_ID" >/dev/null 2>&1; code=$?; set -e
check "$code" "3" "删除后 instance get → exit 3"
set +e; cli instance get "ins_no-such-instance" >/dev/null 2>&1; code=$?; set -e
check "$code" "3" "不存在的实例 get → exit 3"

echo "[7/12] extension：list / system 锁定 403→exit 6 / 非 TTY 缺 --yes→exit 2"
ISARR=$(cli extension list --json | jqpy "1 if isinstance(d, list) else 0")
check "$ISARR" "1" "extension list --json 为数组"
SYS_ID=$(cli extension list --json | jqpy "next((e['id'] for e in d if e['type']=='system'), '')")
[ -n "$SYS_ID" ] && ok "发现 system 扩展 $SYS_ID" || bad "未发现 system 扩展"
set +e; LOCKED=$(cli extension disable "$SYS_ID" --json 2>/dev/null); code=$?; set -e
check "$code" "6" "disable system 扩展 → exit 6"
ERRCODE=$(echo "$LOCKED" | jqpy "d['error']['code']")
check "$ERRCODE" "EXTENSION_SYSTEM_LOCKED" "--json 错误体 error.code==EXTENSION_SYSTEM_LOCKED"
set +e; cli extension remove "$SYS_ID" </dev/null >/dev/null 2>&1; code=$?; set -e
check "$code" "2" "非 TTY remove 无 --yes → exit 2"

echo "[8/12] runtime version + status"
PIN=$(cli runtime version --json | jqpy "1 if d.get('pinnedVersion') else 0")
check "$PIN" "1" "runtime version --json pinnedVersion 非空"
CNT=$(cli status --json | jqpy "1 if d.get('counts') else 0")
check "$CNT" "1" "status --json 含 counts 字段"

echo "[9/12] doctor"
CHK=$(cli doctor --json | jqpy "1 if isinstance(d.get('checks'), list) else 0")
check "$CHK" "1" "doctor --json 含 checks 数组"
set +e; cli doctor >/dev/null 2>&1; code=$?; set -e
if [ "$code" = "0" ] || [ "$code" = "1" ]; then
  ok "doctor exit=${code}（0 全绿 / 1 存在失败项，均合法）"
else
  bad "doctor exit=${code}（只允许 0 或 1）"
fi

echo "[10/12] 清理：env delete --yes + 确认删除"
set +e; cli env delete "$ENV_ID" --yes >/dev/null 2>&1; code=$?; set -e
check "$code" "0" "env delete --yes → exit 0"
set +e; cli env get "$ENV_ID" >/dev/null 2>&1; code=$?; set -e
check "$code" "3" "env get 已删环境 → exit 3 确认删除"

echo "[11/12] 全量能力命令：自建 fixture 自清（tabs/navigate/focus/status + env activity/status + settings 复原）"
ENV2_ID=$(cli env create "smoke11-$(date +%s)" --quiet 2>/dev/null)
[ -n "$ENV2_ID" ] && ok "smoke11 fixture 环境 ${ENV2_ID}" || bad "smoke11 fixture 环境创建失败"
if [ -n "$ENV2_ID" ]; then
  INS2_ID=$(cli instance create "$ENV2_ID" --quiet 2>/dev/null)
  if [ -n "$INS2_ID" ]; then
    ok "smoke11 fixture 实例 ${INS2_ID}"
    TABS_OK=$(cli instance tabs "$INS2_ID" --json | jqpy "1 if isinstance(d,list) and len(d)>=1 and all(('type' in t and 'url' in t) for t in d) else 0")
    check "$TABS_OK" "1" "instance tabs --json：数组且 ≥1 项含 type/url 字段"
    TAB_ID=$(cli instance tabs "$INS2_ID" --json | jqpy "next((t['id'] for t in d if t.get('type')=='page'), '')")
    if [ -z "$TAB_ID" ]; then
      # 起始页 about:blank 也是 page 标签，正常非空；意外为空则先 open 一个 URL 再取
      cli instance open "$INS2_ID" "https://example.com" >/dev/null 2>&1 || true
      TAB_ID=$(cli instance tabs "$INS2_ID" --json | jqpy "next((t['id'] for t in d if t.get('type')=='page'), '')")
    fi
    [ -n "$TAB_ID" ] && ok "取得 page 标签 ${TAB_ID}" || bad "tabs 无 page 标签（含 open 补救后）"
    set +e; NAV=$(cli instance navigate "$INS2_ID" "$TAB_ID" "https://example.com" --json 2>/dev/null); code=$?; set -e
    check "$code" "0" "instance navigate → exit 0"
    NAVOK=$(echo "$NAV" | jqpy "1 if d.get('type')=='page' and d.get('id') and 'url' in d else 0")
    check "$NAVOK" "1" "navigate 返回 target（page，含 id/url）"
    set +e; cli instance focus "$INS2_ID" >/dev/null 2>&1; code=$?; set -e
    check "$code" "0" "instance focus → exit 0"
    ISST=$(cli instance status "$INS2_ID" --json | jqpy "1 if set(d.keys()) >= {'id','status','pid','cdpPort','browserVersion'} else 0")
    check "$ISST" "1" "instance status --json 含五键 id/status/pid/cdpPort/browserVersion"
    EVST=$(cli env status "$ENV2_ID" --json | jqpy "1 if set(d.get('instances',{}).keys()) >= {'total','running','starting','stopped','error'} else 0")
    check "$EVST" "1" "env status --json 含五计数键 total/running/starting/stopped/error"
    set +e; cli env activity "$ENV2_ID" --limit 5 >/dev/null 2>&1; code=$?; set -e
    check "$code" "0" "env activity --limit 5 → exit 0"

    ORIG_URL=$(cli settings get --json | jqpy "d.get('defaultStartUrl','')")
    set +e; cli settings update --start-url "https://smoke11.example.com" >/dev/null 2>&1; code=$?; set -e
    check "$code" "0" "settings update --start-url → exit 0"
    GOT_URL=$(cli settings get --json | jqpy "d.get('defaultStartUrl','')")
    check "$GOT_URL" "https://smoke11.example.com" "settings get：defaultStartUrl 已更新"
    set +e; cli settings update --start-url "$ORIG_URL" >/dev/null 2>&1; code=$?; set -e
    check "$code" "0" "settings update 恢复原值 → exit 0"
    GOT_URL=$(cli settings get --json | jqpy "d.get('defaultStartUrl','')")
    check "$GOT_URL" "$ORIG_URL" "settings get：defaultStartUrl 已复原（原值 ${ORIG_URL}）"
  else
    bad "smoke11 fixture 实例创建失败"
  fi
  # —— 段内自清（不依赖既有清理段）——
  if [ -n "$INS2_ID" ]; then
    cli instance stop "$INS2_ID" >/dev/null 2>&1 || true
    cli instance delete "$INS2_ID" --yes >/dev/null 2>&1 || true
  fi
  cli env delete "$ENV2_ID" --yes >/dev/null 2>&1 || true
  set +e; cli env get "$ENV2_ID" >/dev/null 2>&1; code=$?; set -e
  check "$code" "3" "段内自清：env get 已删环境 → exit 3"
fi

echo "[12/12] 登录态与内核安装（只读收尾）：login-profile list/get + runtime install 幂等/取消"
ISARR=$(cli login-profile list --json | jqpy "1 if isinstance(d, list) else 0")
check "$ISARR" "1" "login-profile list --json 为数组"
LP_ENV=$(cli login-profile list --json | jqpy "d[0]['environmentId'] if d else ''")
[ -z "$LP_ENV" ] && LP_ENV=$(cli env list --json | jqpy "d[0]['id'] if d else ''")
LP_TMP_ENV=""
if [ -z "$LP_ENV" ]; then
  # 全新安装无任何环境：段内自建一次性 fixture（不碰实例，用完即删）
  LP_TMP_ENV=$(cli env create "smoke12-$(date +%s)" --quiet 2>/dev/null)
  LP_ENV="$LP_TMP_ENV"
fi
if [ -n "$LP_ENV" ]; then
  set +e; LPVIEW=$(cli login-profile get "$LP_ENV" --json 2>/dev/null); code=$?; set -e
  check "$code" "0" "login-profile get 既有环境 → exit 0"
  LPST=$(echo "$LPVIEW" | jqpy "d.get('status','')")
  ok "login-profile get：status=${LPST}（not_configured 视图亦合规）"
  if [ -n "$LP_TMP_ENV" ]; then
    cli env delete "$LP_TMP_ENV" --yes >/dev/null 2>&1 || true
  fi
else
  bad "无既有环境且自建 fixture 失败，无法验证 login-profile get"
fi
KINST=$(cli runtime version --json | jqpy "1 if d.get('installed') else 0")
if [ "$KINST" = "1" ]; then
  set +e; cli runtime install >/dev/null 2>&1; code=$?; set -e
  check "$code" "0" "runtime install（已装内核）幂等短路 → exit 0"
else
  echo "  - 内核未安装，跳过 runtime install 幂等短路（冒烟禁止触发真实下载）"
fi
set +e; cli runtime install --cancel --yes >/dev/null 2>&1; code=$?; set -e
check "$code" "0" "runtime install --cancel --yes（无下载 → ok=false）→ exit 0"

echo
echo "结果: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = "0" ]
