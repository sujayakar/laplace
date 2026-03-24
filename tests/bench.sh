#!/usr/bin/env bash
# Benchmark various workloads via serve mode with detailed timing.
set -euo pipefail

TEMPLATE="${1:-/tmp/kvm-v8-template}"
HOST_BIN="./target/release/convex-hypervisor"
RUNS=10

if [ ! -f "$TEMPLATE/cpu.state" ]; then
    echo "Template not found at $TEMPLATE"
    exit 1
fi

bench() {
    local name="$1"
    local js="$2"

    # Generate N copies of the JS
    local input=""
    for i in $(seq 1 $RUNS); do
        input+="$js"$'\n'
    done

    # Run and extract fork times
    local times=$(echo "$input" | $HOST_BIN serve-linux "$TEMPLATE" 2>&1 | grep "fork:" | awk '{print $2}')

    local avg=$(echo "$times" | awk '{sum+=$1; count++} END {printf "%.1f", sum/count}')
    local sorted=$(echo "$times" | sort -n)
    local p50=$(echo "$sorted" | sed -n "$((RUNS/2 + 1))p")
    local min=$(echo "$sorted" | head -1)
    local max=$(echo "$sorted" | tail -1)

    printf "  %-35s avg=%5sms  p50=%5sms  min=%5sms  max=%5sms\n" "$name" "$avg" "$p50" "$min" "$max"
}

echo "=== Workload Benchmarks (${RUNS} runs each) ==="
echo ""

echo "── Minimal ──"
bench "noop (void 0)" "void 0"
bench "console.log(1+1)" "console.log(1+1)"

echo ""
echo "── Realistic: data processing ──"
bench "JSON parse+stringify (small)" "var o={users:[{id:1,name:'Alice'},{id:2,name:'Bob'}]}; console.log(JSON.stringify(JSON.parse(JSON.stringify(o))))"
bench "JSON parse+stringify (medium)" "var a=Array.from({length:100},(_,i)=>({id:i,name:'user'+i,score:Math.random()})); console.log(JSON.stringify(a).length)"
bench "array sort 1K" "var a=Array.from({length:1000},(_,i)=>1000-i); a.sort((a,b)=>a-b); console.log(a[0],a[999])"
bench "array sort 10K" "var a=Array.from({length:10000},(_,i)=>10000-i); a.sort((a,b)=>a-b); console.log(a[0],a[9999])"
bench "map/filter/reduce 10K" "var r=Array.from({length:10000},(_,i)=>i).map(x=>x*x).filter(x=>x%3===0).reduce((s,x)=>s+x,0); console.log(r)"

echo ""
echo "── Realistic: string processing ──"
bench "regex match" "var s='Hello World 2026-03-24'; console.log(s.match(/\\d{4}-\\d{2}-\\d{2}/)[0])"
bench "string split+join 1K" "var s=Array.from({length:1000},(_,i)=>'word'+i).join(' '); console.log(s.split(' ').length)"
bench "template literal" "var name='world'; var n=42; console.log(\`hello \${name}, the answer is \${n}\`)"

echo ""
echo "── CPU-intensive ──"
bench "fibonacci(25)" "function fib(n){return n<=1?n:fib(n-1)+fib(n-2)} console.log(fib(25))"
bench "fibonacci(30)" "function fib(n){return n<=1?n:fib(n-1)+fib(n-2)} console.log(fib(30))"
bench "fibonacci(35)" "function fib(n){return n<=1?n:fib(n-1)+fib(n-2)} console.log(fib(35))"
bench "sieve of eratosthenes (10K)" "function sieve(n){var a=Array(n+1).fill(true);a[0]=a[1]=false;for(var i=2;i*i<=n;i++)if(a[i])for(var j=i*i;j<=n;j+=i)a[j]=false;return a.filter(x=>x).length}console.log(sieve(10000))"
bench "sieve of eratosthenes (100K)" "function sieve(n){var a=Array(n+1).fill(true);a[0]=a[1]=false;for(var i=2;i*i<=n;i++)if(a[i])for(var j=i*i;j<=n;j+=i)a[j]=false;return a.filter(x=>x).length}console.log(sieve(100000))"
bench "SHA-like hash (1K rounds)" "function hash(s){var h=0;for(var i=0;i<s.length;i++){h=((h<<5)-h)+s.charCodeAt(i);h|=0}return h} var r=0;for(var i=0;i<1000;i++)r=hash('input'+r); console.log(r)"

echo ""
echo "── Memory-intensive ──"
bench "allocate 10K objects" "var a=[];for(var i=0;i<10000;i++)a.push({x:i,y:i*i}); console.log(a.length)"
bench "allocate 100K objects" "var a=[];for(var i=0;i<100000;i++)a.push({x:i,y:i*i}); console.log(a.length)"
bench "large typed array (1M)" "var a=new Float64Array(1000000); for(var i=0;i<1000000;i++)a[i]=Math.sin(i); console.log(a[0],a[999999])"

echo ""
echo "── Simulated Convex workload ──"
bench "query+transform+respond" "var users=[{id:1,name:'Alice',age:30},{id:2,name:'Bob',age:25}]; var result=users.filter(u=>u.age>20).map(u=>({...u,greeting:'Hello '+u.name})); console.log(JSON.stringify(result))"
bench "validation+transform" "function validate(o){if(!o.name)throw new Error('no name');if(o.age<0)throw new Error('bad age');return true} var items=Array.from({length:100},(_,i)=>({name:'u'+i,age:20+i})); items.forEach(validate); console.log(items.length+' validated')"
