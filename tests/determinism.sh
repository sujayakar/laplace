#!/usr/bin/env bash
# Determinism fuzzing: run each JS test case N times from the same snapshot,
# verify all runs produce identical output.
set -euo pipefail

TEMPLATE="${1:-/tmp/kvm-v8-template}"
RUNS="${2:-5}"
HOST_BIN="./target/release/convex-hypervisor"
PASS=0
FAIL=0
SKIP=0

if [ ! -f "$TEMPLATE/cpu.state" ]; then
    echo "Template not found at $TEMPLATE. Run snapshot-linux first."
    exit 1
fi

if [ ! -f "$HOST_BIN" ]; then
    echo "Host binary not found. Run: cargo build -p convex-hypervisor --release"
    exit 1
fi

run_test() {
    local name="$1"
    local js="$2"
    local outputs=()

    for i in $(seq 1 "$RUNS"); do
        out=$($HOST_BIN fork-linux --msg "$js" "$TEMPLATE" 2>/dev/null) || true
        outputs+=("$out")
    done

    # Check all outputs match the first
    local first="${outputs[0]}"
    local all_match=true
    for i in $(seq 1 $((RUNS - 1))); do
        if [ "${outputs[$i]}" != "$first" ]; then
            all_match=false
            break
        fi
    done

    if $all_match; then
        echo "  PASS: $name"
        echo "        output: $(echo "$first" | head -1 | cut -c1-80)"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $name"
        for i in $(seq 0 $((RUNS - 1))); do
            echo "        run $((i+1)): ${outputs[$i]}"
        done
        FAIL=$((FAIL + 1))
    fi
}

echo "=== Determinism Fuzzing ==="
echo "Template: $TEMPLATE"
echo "Runs per test: $RUNS"
echo ""

echo "── Basic operations ──"
run_test "arithmetic" 'console.log(1+2, 3*4, 10/3, 2**32)'
run_test "string ops" 'console.log("hello".toUpperCase(), "world".repeat(3), "abc".split("").reverse().join(""))'
run_test "array ops" 'console.log(JSON.stringify([1,2,3].map(x=>x*x).filter(x=>x>1)))'
run_test "object keys" 'console.log(JSON.stringify(Object.keys({z:1,a:2,m:3,b:4})))'
run_test "JSON roundtrip" 'var o={a:1,b:[2,3],c:{d:"e"}}; console.log(JSON.stringify(JSON.parse(JSON.stringify(o))))'

echo ""
echo "── Date/Time (primary determinism target) ──"
run_test "Date.now()" 'console.log(Date.now())'
run_test "new Date()" 'console.log(new Date().toISOString())'
run_test "Date.now() x3" 'console.log(Date.now(), Date.now(), Date.now())'
run_test "performance-like timing" 'var a=Date.now(); for(var i=0;i<1000;i++){} var b=Date.now(); console.log(b-a)'

echo ""
echo "── Math.random (determinism via seeded PRNG) ──"
run_test "Math.random()" 'console.log(Math.random())'
run_test "Math.random() x5" 'console.log(Array.from({length:5},()=>Math.random()).join(","))'
run_test "random sort" 'var a=[1,2,3,4,5,6,7,8,9,10]; a.sort(()=>Math.random()-0.5); console.log(JSON.stringify(a))'

echo ""
echo "── Computation-heavy (GC pressure, optimizer triggers) ──"
run_test "fibonacci" 'function fib(n){return n<=1?n:fib(n-1)+fib(n-2)} console.log(fib(25))'
run_test "large array" 'var a=Array.from({length:10000},(_,i)=>i*i); console.log(a[9999], a.reduce((s,x)=>s+x,0))'
run_test "string concat" 'var s=""; for(var i=0;i<1000;i++) s+=String.fromCharCode(65+(i%26)); console.log(s.length, s.slice(0,26))'
run_test "object allocation" 'var a=[]; for(var i=0;i<1000;i++) a.push({x:i,y:i*i,s:"val"+i}); console.log(a.length, a[999].y)'
run_test "nested objects" 'function make(d){return d<=0?{v:42}:{l:make(d-1),r:make(d-1)}} var t=make(10); function sum(n){return n.v||sum(n.l)+sum(n.r)} console.log(sum(t))'

echo ""
echo "── Adversarial: designed to expose non-determinism ──"
run_test "tight Date.now loop" 'var times=[]; for(var i=0;i<100;i++) times.push(Date.now()); console.log(JSON.stringify(times))'
run_test "interleaved random+time" 'var r=[]; for(var i=0;i<20;i++) r.push(Math.random(),Date.now()); console.log(JSON.stringify(r))'
run_test "sort stability" 'var a=[{k:1,v:"a"},{k:1,v:"b"},{k:1,v:"c"},{k:2,v:"d"},{k:2,v:"e"}]; a.sort((x,y)=>x.k-y.k); console.log(JSON.stringify(a.map(x=>x.v)))'
run_test "Map iteration order" 'var m=new Map(); for(var i=0;i<20;i++) m.set("k"+i, i*i); console.log(JSON.stringify([...m.entries()]))'
run_test "Set iteration order" 'var s=new Set([5,3,1,4,2,10,8,6,9,7]); console.log(JSON.stringify([...s]))'
run_test "WeakRef (GC timing)" 'var o={x:42}; var w=new WeakRef(o); console.log(w.deref()?.x)'
run_test "Promise microtask" 'var r=[]; Promise.resolve().then(()=>r.push("a")); Promise.resolve().then(()=>r.push("b")); r.push("sync"); console.log(JSON.stringify(r))'
run_test "regex backtracking" 'var s="a".repeat(20)+"b"; console.log(/^(a+)+$/.test(s))'
run_test "toString with side effects" 'var c=0; var o={toString(){return "v"+(c++)}};  console.log(""+o, ""+o, ""+o, c)'

echo ""
echo "── Large computation (optimizer/JIT behavior) ──"
run_test "hot loop (JIT)" 'function f(n){var s=0;for(var i=0;i<n;i++)s+=i;return s} console.log(f(100000))'
run_test "polymorphic calls" 'function add(a,b){return a+b} console.log(add(1,2), add("a","b"), add(1.5,2.5), add(true,false))'
run_test "try/catch in loop" 'var s=0; for(var i=0;i<1000;i++){try{s+=i}catch(e){}} console.log(s)'
run_test "eval" 'console.log(eval("1+2+3+4+5"))'
run_test "typed arrays" 'var a=new Float64Array(100); for(var i=0;i<100;i++)a[i]=Math.sin(i); console.log(a[0],a[50],a[99])'
run_test "BigInt" 'console.log((2n**128n).toString())'

echo ""
echo "── Edge cases ──"
run_test "NaN handling" 'console.log(NaN===NaN, isNaN(0/0), Number.isNaN(undefined))'
run_test "Infinity" 'console.log(1/0, -1/0, Infinity+Infinity, Infinity-Infinity)'
run_test "unicode" 'console.log("🎲".length, "café".normalize("NFD").length, "\u{1F600}")'
run_test "empty output" 'void 0'

echo ""
echo "═══════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "═══════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
