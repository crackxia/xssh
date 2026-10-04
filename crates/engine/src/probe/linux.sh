LC_ALL=C; export LC_ALL
T=; if timeout -k 1 5 true 2>/dev/null; then T="timeout -k 2 10"; elif timeout 5 true 2>/dev/null; then T="timeout 10"; fi
echo @@uname; uname -srmn
echo @@osrel; grep -E '^(PRETTY_NAME|ID|VERSION_ID)=' /etc/os-release 2>/dev/null
echo @@uptime; cat /proc/uptime
echo @@nproc; nproc 2>/dev/null || grep -c ^processor /proc/cpuinfo
echo @@loadavg; cat /proc/loadavg
echo @@t1; date +%s.%N
echo @@stat1; grep '^cpu ' /proc/stat
echo @@net1; tail -n +3 /proc/net/dev
echo @@disk1; cat /proc/diskstats 2>/dev/null
sleep 1
echo @@t2; date +%s.%N
echo @@stat2; grep '^cpu ' /proc/stat
echo @@net2; tail -n +3 /proc/net/dev
echo @@disk2; cat /proc/diskstats 2>/dev/null
echo @@meminfo; grep -E '^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SwapTotal|SwapFree|Dirty):' /proc/meminfo
# `/` always (in containers it is an overlay), each mount point once, and no bind-mounted single
# files such as a container's /etc/hosts.
DFSEL='$6=="Mounted"{if(h++)next;print;next} !seen[$6]++ && system("test -d \"" $6 "\"")==0'
echo @@df; { $T df -P -k / 2>/dev/null; $T df -P -k -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs 2>/dev/null || $T df -P -k 2>/dev/null; } | awk "$DFSEL"
echo @@dfi; { $T df -P -i / 2>/dev/null; $T df -P -i -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs 2>/dev/null || $T df -P -i 2>/dev/null; } | awk "$DFSEL"
PS="ps -eo pid,user,pcpu,pmem,rss,etimes,stat,comm"
# Leave out this probe: its shell, the shell that started it, and the helpers it spawned.
PSSELF='NR==1 || !($1==me || $1==pp || ($1>me && ($8=="ps"||$8=="head"||$8=="awk"||$8=="timeout"||$8=="sort"||$8=="cut"||$8=="sleep")))'
if $PS --sort=-pcpu >/dev/null 2>&1; then
  echo @@pscpu; $PS --sort=-pcpu 2>/dev/null | awk -v me=$$ -v pp=$PPID "$PSSELF" | head -n __TOP1__
  echo @@psmem; $PS --sort=-rss 2>/dev/null | awk -v me=$$ -v pp=$PPID "$PSSELF" | head -n __TOP1__
  echo @@procs; ps -eo stat= 2>/dev/null | cut -c1 | sort | uniq -c
else
  # busybox/toybox ps: compute the same columns from /proc (pcpu = lifetime average, like procps).
  HZ=$(getconf CLK_TCK 2>/dev/null || echo 100); UP=$(cut -d' ' -f1 /proc/uptime)
  MT=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo); PG=$(getconf PAGESIZE 2>/dev/null || echo 4096)
  X=$(cat /proc/[0-9]*/stat 2>/dev/null | awk -v hz="$HZ" -v up="$UP" -v mt="$MT" -v pg="$PG" '{
    l = $0; i = index(l, "("); j = 0
    for (k = length(l); k > 0; k--) if (substr(l, k, 1) == ")") { j = k; break }
    comm = substr(l, i + 1, j - i - 1); gsub(/ /, "_", comm); n = split(substr(l, j + 2), f, " ")
    el = up - f[20] / hz; if (el < 1) el = 1
    rss = f[22] * pg / 1024; cpu = (f[12] + f[13]) / hz / el * 100; mem = (mt > 0) ? rss * 100 / mt : 0
    printf "%d ? %.1f %.1f %d %d %s %s\n", $1, cpu, mem, rss, el, f[1], comm
  }' 2>/dev/null)
  echo @@pscpu; echo "PID USER %CPU %MEM RSS ELAPSED STAT COMMAND"; printf '%s\n' "$X" | sort -k3 -nr | head -n __TOP1__
  echo @@psmem; echo "PID USER %CPU %MEM RSS ELAPSED STAT COMMAND"; printf '%s\n' "$X" | sort -k5 -nr | head -n __TOP1__
  echo @@procs; printf '%s\n' "$X" | awk 'NF >= 8 {print substr($7, 1, 1)}' | sort | uniq -c
fi
echo @@ports; $T ss -Htlnp 2>/dev/null || $T netstat -tlnp 2>/dev/null | tail -n +3
echo @@failed; command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ] && echo SYSTEMD && $T systemctl --failed --no-legend --plain 2>/dev/null
echo @@docker; command -v docker >/dev/null 2>&1 && $T docker ps --format '{{.Names}}|{{.Status}}|{{.Image}}' 2>/dev/null | head -n 50
echo @@psi; for f in cpu memory io; do [ -r /proc/pressure/$f ] && echo "$f $(head -n 1 /proc/pressure/$f)"; done
# The journal only counts when it actually shows kernel messages (not readable without the
# systemd-journal group); dmesg then covers the whole boot instead of 24h.
echo @@oomsrc; S=none; if [ -n "$($T journalctl -k -b -n 1 --no-pager -q 2>/dev/null)" ]; then S=journal; elif $T dmesg >/dev/null 2>&1; then S=dmesg; fi; echo $S
echo @@oom; case $S in journal) $T journalctl -k --since=-24h --no-pager -q 2>/dev/null ;; dmesg) $T dmesg 2>/dev/null ;; esac | grep -iE 'out of memory|oom-kill|killed process' | tail -n 5
echo @@user; id -un 2>/dev/null
echo @@end
