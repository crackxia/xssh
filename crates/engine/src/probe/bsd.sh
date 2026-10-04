LC_ALL=C; export LC_ALL
OS=$(uname -s)
T=; if timeout 5 true 2>/dev/null; then T="timeout 10"; fi
echo @@uname; uname -srmn
echo @@osrel; if [ "$OS" = Darwin ]; then sw_vers 2>/dev/null; else freebsd-version 2>/dev/null || uname -r; fi
echo @@boottime; sysctl -n kern.boottime
echo @@now; date +%s
echo @@nproc; sysctl -n hw.ncpu
echo @@loadavg; sysctl -n vm.loadavg
echo @@net1; netstat -ibn 2>/dev/null
if [ "$OS" = Darwin ]; then
  echo @@cputop; top -l 2 -n 0 -s 1 2>/dev/null | grep -E '^CPU usage' | tail -n 1
else
  echo @@cp1; sysctl -n kern.cp_time
  sleep 1
  echo @@cp2; sysctl -n kern.cp_time
fi
echo @@net2; netstat -ibn 2>/dev/null
echo @@mem; sysctl -n hw.pagesize; if [ "$OS" = Darwin ]; then sysctl -n hw.memsize; else sysctl -n hw.physmem; fi
if [ "$OS" = Darwin ]; then
  echo @@vmstat; vm_stat
  echo @@swap; sysctl -n vm.swapusage
else
  echo @@bsdmem; sysctl -n vm.stats.vm.v_free_count vm.stats.vm.v_inactive_count vm.stats.vm.v_laundry_count 2>/dev/null
  echo @@swapinfo; swapinfo -k 2>/dev/null | tail -n 1
fi
echo @@df; $T df -P -k 2>/dev/null | grep -vE '^(devfs|map |fdescfs|procfs|tmpfs)'
echo @@pscpu; ps -axo pid,user,pcpu,pmem,rss,etime,stat,comm -r 2>/dev/null | head -n __TOP1__
echo @@psmem; ps -axo pid,user,pcpu,pmem,rss,etime,stat,comm -m 2>/dev/null | head -n __TOP1__
echo @@procs; ps -axo stat= 2>/dev/null | cut -c1 | sort | uniq -c
if [ "$OS" = Darwin ]; then
  echo @@lsof; $T lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | tail -n +2
else
  echo @@sockstat; $T sockstat -46 -l -P tcp 2>/dev/null | tail -n +2
fi
echo @@docker; command -v docker >/dev/null 2>&1 && $T docker ps --format '{{.Names}}|{{.Status}}|{{.Image}}' 2>/dev/null | head -n 50
echo @@user; id -un 2>/dev/null
echo @@end
