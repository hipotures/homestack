/* ECharts presentation bridge. Accounting and query planning belong to Rust. */
"use strict";
(() => {
  const agents = {claude:"Claude Code", codex:"Codex"};
  const widgets = new Map(), datasets = new Map();
  let selection, epoch = 0, snapshot, lastPacket = 0, sourceAt = 0;
  let currentTheme = matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  try { currentTheme = localStorage.getItem("theme") || currentTheme; } catch (_) {}
  const el = (tag, cls, text) => { const e=document.createElement(tag); if(cls)e.className=cls; if(text!==undefined)e.textContent=text;return e; };
  const $ = id => document.getElementById(id);
  // Tooltips and tables show full precision; cards use the short AIUsage.fmt forms.
  const format = (v,unit) => v == null ? "—" : unit === "USD" ? AIUsage.fmt.cost(v,true) : unit === "ratio" ? AIUsage.fmt.pct(v*100,2) : unit === "seconds" ? `${AIUsage.fmt.full(Math.round(v*1000)/1000)} s` : AIUsage.fmt.full(Math.round(v*100)/100);
  function initial() {
    const out={agent:"all",host:"*",project:"*",range:"24h",_more:false,_sync:0,_packet:""};
    try {for(const key of ["agent","host","project","range"]) out[key]=localStorage.getItem(key)||out[key];}catch(_){}
    if(!["all","claude","codex"].includes(out.agent))out.agent="all";
    if(!Object.keys(AIUsage.RANGES).includes(out.range))out.range="24h";
    // Scope options must exist before Datastar binds a remembered selection.
    for(const key of ["host","project"])if(out[key]!=="*"){const o=el("option","",out[key]);o.value=out[key];$(key).append(o);}
    return out;
  }
  function select(next,sync) {
    epoch++; selection={...next}; snapshot=undefined;
    try {for(const [key,value] of Object.entries(next))localStorage.setItem(key,value);}catch(_){}
    return epoch;
  }
  function scopeOptions(id,values,label) {
    const target=$(id),current=selection[id],wanted=["*",...new Set([...values,current].filter(v=>v!=="*"))];
    if(JSON.stringify([...target.options].map(o=>o.value))===JSON.stringify(wanted))return;
    target.replaceChildren(...wanted.map(value=>{const option=el("option","",value==="*"?label:value);option.value=value;option.selected=value===current;return option;}));
  }
  function apply(packet) {
    if(packet.epoch!==epoch)return;
    lastPacket=Date.now();
    if(packet.kind==="error") {$("error").textContent="Cannot load data: "+packet.message;$("error").hidden=false;return;}
    const dirty=new Set();
    if(packet.kind==="full") {
      const full=packet.data;
      if(JSON.stringify(full.selection)!==JSON.stringify(selection)) {
        // Object key ordering is irrelevant to selection identity.
        if(Object.keys(selection).some(k=>selection[k]!==full.selection[k]))return;
      }
      for(const [id,d] of Object.entries(full.datasets))if(datasets.get(id)?.version!==d.version)dirty.add(id);
      for(const id of datasets.keys())if(!full.datasets[id])dirty.add(id);
      datasets.clear();for(const [id,d]of Object.entries(full.datasets))datasets.set(id,d);
      snapshot=full.snapshot;
      scopeOptions("host",full.hosts,"Servers: all");scopeOptions("project",full.projects,"Projects: all");
      freshness(full.freshness);
    } else {
      if(!snapshot||packet.base!==snapshot) {resync();return;}
      // Validate the entire batch before changing any dataset.
      if(Object.keys(selection).some(k=>selection[k]!==packet.selection[k])){resync();return;}
      for(const change of packet.changes){
        if(change.op!=="replace" && datasets.get(change.id)?.version!==change.base){resync();return;}
        if(change.op==="points" && (change.meta.id!==change.id || change.meta.version!==change.version || change.series.some(p=>!datasets.get(change.id).series.some(s=>s.id===p.id)))){resync();return;}
      }
      for(const change of packet.changes) {
        if(change.op==="replace"){datasets.set(change.dataset.id,change.dataset);dirty.add(change.dataset.id);}
        else if(change.op==="remove"){datasets.delete(change.id);dirty.add(change.id);}
        else {
          const d=datasets.get(change.id);
          for(const patch of change.series){const s=d.series.find(s=>s.id===patch.id);if(!s){resync();return;}
            const points=new Map(s.points);for(const t of patch.remove)points.delete(t);for(const [t,v]of patch.set)points.set(t,v);s.points=[...points].sort((a,b)=>a[0]-b[0]);}
          Object.assign(d,change.meta);dirty.add(change.id);
        }
      }
      snapshot=packet.snapshot;freshness(packet.freshness);
      scopeOptions("host",packet.hosts,"Servers: all");scopeOptions("project",packet.projects,"Projects: all");
    }
    $("error").hidden=true;
    render(dirty);
  }
  function resync() {$("error").hidden=false;$("error").textContent="Synchronizing dataset versions…";$("resync").click();}
  function freshness(f) {sourceAt=f.source_at;const d=new Date(f.source_at*1000);$("updated-text").textContent=`live · ${String(d.getHours()).padStart(2,"0")}:${String(d.getMinutes()).padStart(2,"0")}`;$("updated").classList.remove("stale");}
  function theme() {
    document.documentElement.dataset.theme=currentTheme;
    $("theme").textContent=currentTheme==="dark"?"☀ Light":"☾ Dark";
    // Series colors are resolved from CSS tokens, so charts are redrawn after the theme rebuild.
    AIUsage.refresh();
    for(const w of widgets.values())if(w.spec)draw(w);
    if(datasets.size)renderOverview();
  }
  /** The dataset of a measurement the agent reports; agent capabilities come only from data. */
  const reported=(agent,measurement)=>{const d=datasets.get(`${agent}.${measurement}`);return d&&d.state!=="unavailable"?d:null;};
  function definitions() {
    const list=[], selected=chosen();
    const parts=(measurement,projector)=>selected.flatMap(agent=>{
      const d=reported(agent,measurement);
      return d?projector(d).map(s=>({...s,label:s.label||agents[agent],agent,dataset:d.id})):[];
    });
    // Series without a single nonzero value are dropped, and a panel with no series is not shown.
    const add=(id,title,unit,data,options={})=>{data=data.filter(s=>AIUsage.nonEmpty([{data:s.points}]).length);if(data.length)list.push({id,title,unit,series:data,sourceIds:[...new Set(data.map(s=>s.dataset))],...options});};
    const timeline=d=>[{id:d.agent,label:agents[d.agent]+(d.agent==="codex"&&d.measurement==="cost"?" (estimated)":""),points:d.details?.timeline||[]}];
    const byKind=(key,items)=>d=>items(d).map(s=>({...s,label:agents[d.agent]+" · "+key(s)}));
    add("cost","Cost over time","USD",parts("cost",timeline));
    add("models-cost","Cost by model","USD",parts("cost",byKind(s=>s.dimensions.model,d=>d.series)),{stack:true});
    for(const [id,title]of [["total","Tokens over time"],["input","Uncached input tokens"],["cached","Cached input tokens"],["written","Cache write tokens"],["output","Output tokens (includes reasoning)"],["reasoning","Reasoning tokens (subset of output)"]]) {
      add(id,title,"tokens",parts("tokens",d=>(d.details.categories||[]).filter(s=>s.id===id).map(s=>({...s,id:d.agent,label:agents[d.agent]}))));
    }
    add("models-tokens","Tokens by model","tokens",parts("tokens",byKind(s=>s.label,d=>d.details.models||[])),{stack:true});
    add("cacheHit","Cache hit","ratio",parts("tokens",d=>[{id:d.agent,label:agents[d.agent],points:d.details.cacheHit||[]}]),{max:1});
    for(const [measurement,title,unit]of [["sessions","New sessions","count"],["active","Active time","seconds"],["commits","Commits","count"],["lines","Lines of code","count"],["turns","Turns","count"],["tools","Tool calls","count"],["turnDuration","Average turn duration","seconds"],["firstToken","Time to first token","seconds"],["firstOutput","Time to first output item","seconds"],["toolDuration","Average tool duration","seconds"]]) {
      add(measurement,title,unit,parts(measurement,measurement==="lines"?byKind(s=>s.dimensions.kind,d=>d.series):timeline));
    }
    add("active-kind","Active time by kind","seconds",parts("active",byKind(s=>s.dimensions.kind,d=>d.series)));
    add("tool-failures","Failed tool calls","count",parts("tools",d=>[{id:d.agent,points:d.details.failures||[]}]));
    add("tools-kind","Tool calls by tool","count",parts("tools",byKind(s=>s.label,d=>d.details.tools||[])),{stack:true});
    add("turn-time","Total turn duration","seconds",parts("turnDuration",d=>[{id:"total",label:agents[d.agent]+" · total turn duration",points:d.details.totalTimeline||[]}]));
    const activity=datasets.get("all.activity");
    // The heatmap reads a separate hourly block capped at 30 days (7 at least), so its window is stated.
    if(activity)add("activity","Activity by local weekday/hour","tokens",activity.series.map(s=>({...s,dataset:activity.id})),{heat:true,note:`last ${Math.round((activity.range.end-activity.range.start)/86400)} days`});
    return list;
  }
  function render(dirty) {
    if(dirty.size)renderOverview();
    const definitionsNow=definitions(),ids=new Set(definitionsNow.map(s=>s.id));
    for(const [id,w]of widgets)if(!ids.has(id)){w.chart?.dispose();w.card.remove();widgets.delete(id);}
    for(const spec of definitionsNow) {
      let w=widgets.get(spec.id);
      if(!w){w=create(spec);widgets.set(spec.id,w);w.spec=spec;draw(w);}
      else if([...w.spec.sourceIds,...spec.sourceIds].some(id=>dirty.has(id))){w.spec=spec;draw(w);}
    }
    $("raw-toggle").textContent=`${$("raw").hidden?"Show":"Hide"} raw charts (${widgets.size})`;
  }
  function create(spec) {
    const card=el("article","card"),head=el("div","card-head"),actions=el("div","card-actions"),toggle=el("button","toggle","table"),copy=el("button","copy","⧉"),note=el("div","note"),chartHost=el("div","chart"),tableHost=el("div","table-wrap");
    card.id="widget-"+spec.id;chartHost.id="chart-"+spec.id;chartHost.setAttribute("data-ignore-morph","");chartHost.setAttribute("role","img");chartHost.setAttribute("aria-label",spec.title);
    copy.type=toggle.type="button";copy.hidden=true;copy.title="Copy data as JSON";copy.setAttribute("aria-label",copy.title);
    head.append(el("h2","",spec.title),el("span","unit",spec.unit),actions);actions.append(copy,toggle);card.append(head,note,chartHost,tableHost);$("cards").append(card);tableHost.hidden=true;
    const w={card,head,note,copy,host:chartHost,tableHost,table:false,spec,chart:null,updates:0};
    toggle.onclick=()=>{w.table=!w.table;toggle.textContent=w.table?"chart":"table";draw(w);if(!w.table)w.chart?.resize();};
    copy.onclick=()=>copyCharts(copy,[w.spec]);
    return w;
  }
  function draw(w) {
    const spec=w.spec;
    w.copy.hidden=!w.table;
    const timeRange=datasets.get(spec.series[0]?.dataset)?.range;
    const sources=spec.sourceIds.map(id=>datasets.get(id)).filter(Boolean);
    // The time axis starts at the first bucket with data; a chip tells when that is later than the range.
    const first=firstData(spec.series),since=!spec.heat&&timeRange&&first>timeRange.start?first:null;
    w.note.replaceChildren(...[since?el("span","chip",`data since ${sinceLabel(since,timeRange)}`):null,spec.note,sources.some(d=>d.state==="partial")?"Partial data":""].filter(Boolean).map(n=>typeof n==="string"?el("span","",n):n));
    w.note.hidden=!w.note.childElementCount;
    w.host.hidden=w.table;
    w.tableHost.hidden=!w.table;
    if(w.table){renderTable(w);return;}
    if(!w.chart)w.chart=AIUsage.init(w.host);
    let option;
    if(spec.heat) {
      const cells=new Map();for(const s of spec.series)for(const [t,v]of s.points){const d=new Date(t*1000),key=`${(d.getDay()+6)%7},${d.getHours()}`;cells.set(key,(cells.get(key)||0)+v);}
      const data=[...cells].map(([key,v])=>{const [day,hour]=key.split(',').map(Number);return [hour,day,v];});
      option={tooltip:{position:"top"},grid:{left:12,right:16,top:16,bottom:74,containLabel:true},xAxis:{type:"category",axisLabel:{interval:"auto",hideOverlap:true,margin:10},axisTick:{alignWithLabel:true},data:Array.from({length:24},(_,h)=>String(h).padStart(2,"0"))},yAxis:{type:"category",axisLabel:{interval:0,margin:8},data:["Mon","Tue","Wed","Thu","Fri","Sat","Sun"]},visualMap:{min:0,max:Math.max(1,...data.map(v=>v[2])),calculable:true,orient:"horizontal",left:"center",bottom:20,itemWidth:12,itemHeight:120,padding:8,formatter:value=>AIUsage.fmt.tokens(value)},series:[{id:"heat",type:"heatmap",data,emphasis:{itemStyle:{borderWidth:1}}}]};
    } else {
      option={grid:{left:65,right:18,top:55,bottom:55},legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0},tooltip:{trigger:"axis",axisPointer:{type:"shadow"},valueFormatter:v=>format(v,spec.unit)},xAxis:{type:"time",min:timeRange?((since??timeRange.start)-timeRange.step/2)*1000:undefined,max:timeRange?timeRange.end*1000:undefined,axisLabel:{hideOverlap:true},axisPointer:{label:{formatter:p=>new Date(p.value).toLocaleString()}}},yAxis:{type:"value",max:spec.max,minInterval:["count","tokens"].includes(spec.unit)?1:undefined,axisLabel:{formatter:v=>spec.unit==="ratio"?`${Math.round(v*100)}%`:spec.unit==="USD"?AIUsage.fmt.cost(v):spec.unit==="seconds"?AIUsage.fmt.duration(v):AIUsage.fmt.tokens(v)}},dataZoom:[{type:"inside"}],series:spec.series.map(s=>({id:`${s.dataset}:${s.id}`,name:s.label||s.id,type:spec.unit==="ratio"?"line":"bar",stack:spec.stack?"total":undefined,itemStyle:{color:seriesColor(s)},lineStyle:{color:seriesColor(s)},data:s.points.map(([t,v])=>[t*1000,v]),connectNulls:false}))};
    }
    Object.assign(option,{animationDurationUpdate:180,aria:{enabled:true}});
    w.chart.setOption(option,{replaceMerge:["series"],lazyUpdate:true});w.updates++;
  }
  // "Agent · model" (or kind) series get a shade of the agent hue; everything else the agent hue.
  const seriesColor=s=>AIUsage.colorFor(s.label?.includes(" · ")?s.label:agents[s.agent]||s.label||s.id);
  const firstData=series=>Math.min(...series.flatMap(s=>s.points.filter(([,v])=>v>0).map(([t])=>t)));
  const sinceLabel=(t,range)=>{const d=new Date(t*1000),two=n=>String(n).padStart(2,"0");return range.end-range.start<=86400+range.step?`${two(d.getHours())}:${two(d.getMinutes())}`:`${two(d.getDate())}.${two(d.getMonth()+1)}`;};
  function renderTable(w) {
    const table=el("table"),head=el("tr");head.append(el("th","","Bucket start (local)"),...w.spec.series.map(s=>el("th","",s.label||s.id)));table.append(head);
    const times=[...new Set(w.spec.series.flatMap(s=>s.points.map(p=>p[0])))].sort((a,b)=>b-a),maps=w.spec.series.map(s=>new Map(s.points));
    for(const t of times){const row=el("tr");row.append(el("td","",new Date(t*1000).toLocaleString()),...maps.map(m=>el("td","",format(m.get(t),w.spec.unit))));table.append(row);}
    w.tableHost.replaceChildren(table);
  }
  // ---- Overview (main view). Values are backend totals; the browser only lays them out. ----
  const F=AIUsage.fmt, css=name=>`var(${name})`;
  const esc=v=>String(v).replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"})[c]);
  const two=n=>String(n).padStart(2,"0");
  const chosen=()=>Object.keys(agents).filter(a=>selection.agent==="all"||selection.agent===a);
  let metric="cost",trendChart,heatChart;
  try{metric=["tokens","types"].includes(localStorage.getItem("metric"))?localStorage.getItem("metric"):"cost";}catch(_){}
  // Token types stack to the total: output is drawn without its reasoning part.
  const TYPES=[["cached","Cached input","--tok-cached"],["input","Uncached input","--tok-uncached"],["written","Cache write","--codex-lo"],["output","Output","--tok-output"],["reasoning","Reasoning","--tok-reasoning"]];
  /** Bucket times of the current range, oldest first. */
  const buckets=range=>{const out=[];for(let t=range.start;t<range.end;t+=range.step)out.push(t);return out;};
  const bucketLabel=step=>step<3600?`${step/60}-minute buckets`:step<86400?`${step/3600}-hour buckets`:step===86400?"daily buckets":step===604800?"weekly buckets":`${step/86400}-day buckets`;
  function timeLabel(t,range){
    const d=new Date(t*1000),hm=`${two(d.getHours())}:${two(d.getMinutes())}`,dm=`${two(d.getDate())}.${two(d.getMonth()+1)}`;
    return range.end-range.start<=86400+range.step?hm:range.step<86400?`${dm} ${hm}`:dm;
  }
  /** Sums point lists by time; used only to draw combined sparklines and stacks. */
  function sumPoints(lists){const m=new Map();for(const list of lists)for(const [t,v]of list||[])m.set(t,(m.get(t)||0)+v);return m;}
  function delta(current,previous,{goodUp=false,points=false}={}){
    if(current==null||previous==null||(!points&&!previous))return "";
    const change=points?(current-previous)*100:(current-previous)/previous*100;
    if(Math.abs(change)<(points?0.05:0.5))return `<span class="delta faint" title="vs previous period">±0${points?" pp":"%"}</span>`;
    const up=change>0,text=points?`${Math.abs(change).toFixed(1)} pp`:`${Math.abs(change)>=100?Math.round(Math.abs(change)):Math.abs(change).toFixed(0)}%`;
    return `<span class="delta ${up===goodUp?"down":"up"}" title="vs previous period of the same length">${up?"▲":"▼"} ${text}</span>`;
  }
  function split(key,summary){
    const parts=chosen().map(a=>({a,v:summary[a]?.[key]?.value})),total=parts.reduce((s,p)=>s+(p.v||0),0);
    if(!total)return "";
    return `<div class="stack"><div class="split" aria-hidden="true">${parts.map(p=>`<span style="width:${(p.v||0)/total*100}%;background:${AIUsage.colorFor(agents[p.a])}"></span>`).join("")}</div>
      <div class="legend-row">${parts.map(p=>`<span><span style="color:${AIUsage.colorFor(agents[p.a])}">●</span> ${agents[p.a]} ${p.v==null?"not reported":Math.round(p.v/total*100)+"%"}</span>`).join("")}</div></div>`;
  }
  function sparkline(points,color){
    const values=points.map(([,v])=>v),max=Math.max(...values,0);
    if(values.length<2||!max)return "";
    const xy=points.map(([,v],i)=>[i*300/(points.length-1),34-v/max*30]),line=xy.map(([x,y],i)=>`${i?"L":"M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
    return `<svg class="spark" viewBox="0 0 300 36" preserveAspectRatio="none" aria-hidden="true"><path d="${line} L300 36 L0 36 Z" fill="${color}" fill-opacity=".14"></path><path d="${line}" fill="none" stroke="${color}" stroke-width="1.5" vector-effect="non-scaling-stroke"></path></svg>`;
  }
  function renderOverview() {
    const summary=datasets.get("all.summary")?.details;
    if(!summary)return;
    const totals=summary.totals||{},previous=Object.fromEntries(Object.entries(summary.previous?.totals||{}).map(([k,v])=>[k,v.value])),range=datasets.get("all.summary").range;
    const sel=chosen(),tokens=sel.map(a=>datasets.get(`${a}.tokens`)).filter(d=>d&&d.state!=="unavailable");
    const category=id=>tokens.map(d=>(d.details.categories||[]).find(c=>c.id===id)?.points);
    const lead=sel.length===1?AIUsage.colorFor(agents[sel[0]]):css("--text-2");
    const kpi=(label,value,deltaHtml,body,title="",full="")=>`<div class="card kpi"><div class="eyebrow" title="${esc(title)}">${label}</div><div class="figure"><span class="kpi-value${value==="—"?" faint":""}" title="${esc(full)}">${value}</span>${deltaHtml}</div>${body}</div>`;
    const totalSeries=sumPoints(category("total")),firstActive=Math.min(...[...totalSeries].filter(([,v])=>v>0).map(([t])=>t));
    const live=buckets(range).filter(t=>t>=firstActive||!Number.isFinite(firstActive));
    // Cache hit per bucket over all selected agents, drawn on a narrow scale.
    const sums=["input","cached","written"].map(id=>sumPoints(category(id)));
    const hits=live.map(t=>{const all=sums.reduce((n,m)=>n+(m.get(t)||0),0);return [t,all>0?(sums[1].get(t)||0)/all*100:null];});
    const known=hits.filter(([,v])=>v!=null),low=known.reduce((m,h)=>h[1]<m[1]?h:m,[0,101]);
    // Near-empty buckets can drop to 0%, so the scale follows the range-wide rate and lower buckets are clamped.
    const overall=(totals.cacheHit?.value??0)*100,floor=overall>=98?98:Math.max(0,Math.floor(overall/5)*5-5);
    const hitBars=known.length?`<div class="stack"><div class="hits" aria-hidden="true">${hits.map(([,v])=>v==null?`<span style="height:2px;background:var(--track)"></span>`:`<span style="height:${Math.max(2,Math.round((Math.max(v,floor)-floor)/(100-floor)*24))}px;background:${v<Math.max(floor,overall-1)?css("--warn"):css("--codex")}"></span>`).join("")}</div>
      <div class="note">scale ${floor}–100% · min ${F.pct(low[1])} at ${timeLabel(low[0],range)}</div></div>`:"";
    const output=totals.output?.value,reasoning=totals.reasoning?.value,share=output&&reasoning!=null?reasoning/output:null;
    const outputBody=output?`<div class="stack"><div class="split" aria-hidden="true"><span style="width:${(share||0)*100}%;background:var(--tok-reasoning)"></span><span style="width:${(1-(share||0))*100}%;background:var(--tok-output)"></span></div>
      <div class="legend-row"><span>${share==null?"reasoning not reported":`<span style="color:var(--tok-reasoning)">●</span> reasoning ${F.tokens(reasoning)} · ${Math.round(share*100)}%`}</span><span>response ${F.tokens(output-(reasoning||0))}</span></div></div>`:"";
    const partial=key=>{const n=totals[key]?.contributors??0;return n&&n<sel.length?" (partial)":"";};
    $("kpis").innerHTML=[
      kpi("Cost"+partial("cost"),F.cost(totals.cost?.value),delta(totals.cost?.value,previous.cost),split("cost",summary),"Codex cost is estimated from standard API prices",F.cost(totals.cost?.value,true)),
      kpi("Tokens"+partial("total"),F.tokens(totals.total?.value),delta(totals.total?.value,previous.total),sparkline(live.map(t=>[t,totalSeries.get(t)||0]),lead)+split("total",summary),"",F.full(totals.total?.value)),
      kpi("Cache hit",totals.cacheHit?.value==null?"—":F.pct(totals.cacheHit.value*100),delta(totals.cacheHit?.value,previous.cacheHit,{goodUp:true,points:true}),hitBars),
      kpi("Output",F.tokens(output),delta(output,previous.output),outputBody,"Output includes reasoning tokens",F.full(output)),
    ].join("");
    renderTrend(range,firstActive);renderComposition(totals);renderModels();renderHeat();renderSpikes(summary,range);renderAgents(summary,range);
  }
  function renderTrend(range,firstActive) {
    const sel=chosen(),series=[];
    if(metric==="types"){
      const kind=id=>sumPoints(sel.map(a=>datasets.get(`${a}.tokens`)).filter(d=>d&&d.state!=="unavailable").map(d=>(d.details.categories||[]).find(c=>c.id===id)?.points));
      const reasoning=kind("reasoning");
      for(const [id,name,color]of TYPES)series.push({name,color:getComputedStyle(document.documentElement).getPropertyValue(color).trim(),points:[...kind(id)].map(([t,v])=>[t,id==="output"?v-(reasoning.get(t)||0):v])});
    }
    else for(const a of sel){
      const d=datasets.get(`${a}.${metric==="cost"?"cost":"tokens"}`);
      if(!d||d.state==="unavailable")continue;
      const rows=metric==="cost"?d.series.map(s=>({model:s.dimensions.model,points:s.points})):(d.details.models||[]).map(s=>({model:s.id,points:s.points}));
      for(const r of rows)series.push({name:`${agents[a]} · ${r.model||"?"}`,points:r.points});
    }
    const visible=AIUsage.nonEmpty(series.map(s=>({...s,data:s.points}))),times=buckets(range);
    // Panels without any nonempty series are hidden rather than drawn empty.
    $("trend").hidden=!visible.length;if(!visible.length)return;
    const stack=sumPoints(visible.map(s=>s.points)),first=Number.isFinite(firstActive)?firstActive:range.start;
    const fmtShort=metric==="cost"?v=>F.cost(v):F.tokens,fmtFull=metric==="cost"?v=>F.cost(v,true):F.full;
    $("trend-title").textContent=metric==="cost"?"Cost over time":"Tokens over time";
    $("trend-sub").textContent=`${bucketLabel(range.step)} · stacked by ${metric==="types"?"token type":"model"}`;
    for(const b of document.querySelectorAll("[data-metric]"))b.setAttribute("aria-pressed",String(b.dataset.metric===metric));
    $("trend-legend").innerHTML=visible.map(s=>`<span><i class="sw" style="background:${s.color||AIUsage.colorFor(s.name)}"></i>${esc(s.name)}</span>`).join("");
    $("trend-chips").innerHTML=first>range.start?`<span class="chip">data since ${timeLabel(first,range)}</span>`:"";
    // Runs of at least three empty buckets inside the data span are marked as gaps.
    const gaps=[];let run=null;
    for(const t of times.filter(t=>t>=first&&t<range.partial_start)){if(!(stack.get(t)>0)){run=run||[t,t];run[1]=t;}else{if(run&&(run[1]-run[0])/range.step>=2)gaps.push(run);run=null;}}
    const peak=[...stack].filter(([t])=>t>=first).reduce((m,p)=>p[1]>m[1]?p:m,[0,0]);
    trendChart=trendChart||AIUsage.init($("trend-chart"));
    trendChart.setOption({
      grid:{left:8,right:8,top:36,bottom:8,containLabel:true},
      tooltip:{trigger:"axis",formatter:AIUsage.tooltipFormatter(fmtFull)},
      xAxis:{type:"time",min:(first-range.step/2)*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",axisLabel:{formatter:fmtShort}},
      // Stacks on a time axis need identical x values in every series, so all are aligned to the buckets.
      series:visible.map((s,i)=>AIUsage.bar(s.name,(m=>times.filter(t=>t>=first).map(t=>[t*1000,m.get(t)||0]))(new Map(s.points)),s.name,{
        id:s.name,barMaxWidth:28,...(s.color?{itemStyle:{color:s.color}}:{}),
        markPoint:i===visible.length-1&&peak[1]>0?{...AIUsage.peak(()=>`peak ${timeLabel(peak[0],range)}  ${fmtShort(peak[1])}`),data:[{coord:[peak[0]*1000,peak[1]]}]}:undefined,
        markArea:i===0&&gaps.length?{...AIUsage.gaps([]),data:gaps.map(([a,b])=>[{xAxis:(a-range.step/2)*1000,name:`gap ${timeLabel(a,range)}–${timeLabel(b+range.step,range)}`},{xAxis:(b+range.step/2)*1000}])}:undefined,
      })),
    },{replaceMerge:["series"]});
  }
  function renderComposition(totals) {
    const v=k=>totals[k]?.value||0,reasoning=totals.reasoning?.value;
    const rows=[["Cached input",v("cached"),"--tok-cached"],["Uncached input",v("input"),"--tok-uncached"],["Cache write",v("written"),"--codex-lo"],
      [reasoning==null?"Output":"Output (excl. reasoning)",v("output")-(reasoning||0),"--tok-output"],...(reasoning==null?[]:[["Reasoning",reasoning,"--tok-reasoning"]])];
    const all=rows.reduce((s,r)=>s+r[1],0),shown=rows.filter(r=>r[1]>0);
    const ratio=v("output")?Math.round(v("cached")/v("output")):null;
    $("composition").hidden=!all;
    $("composition").innerHTML=`<div><h2>Token composition</h2>${ratio!=null?`<div class="sub">each output token comes with <b style="color:var(--text)">${F.full(ratio)}</b> cached input tokens</div>`:""}</div>
      ${all?`<div class="bars">${shown.map(([label,n,color])=>{const p=n/all*100;return `<div class="bar-row"><div class="head"><span>${label}</span><span class="num">${F.full(n)} <span class="faint">${p<1?p.toFixed(2):p.toFixed(1)}%</span></span></div><div class="track" aria-hidden="true"><span style="width:${Math.max(0.8,p)}%;background:var(${color})"></span></div></div>`;}).join("")}</div>`:`<div class="note">No token usage in this range.</div>`}
      ${all&&!v("written")?`<div class="note faint">Cache write: 0, hidden because there is no data in this range.</div>`:""}${reasoning==null&&all?`<div class="note faint">Reasoning is not reported by the selected agents.</div>`:""}`;
  }
  function renderModels() {
    const rows=[];
    for(const a of chosen()){
      const cost=datasets.get(`${a}.cost`),tokens=datasets.get(`${a}.tokens`),byModel=new Map();
      const entry=m=>{if(!byModel.has(m))byModel.set(m,{agent:a,model:m||"?",cost:null,tokens:0});return byModel.get(m);};
      for(const s of cost?.series||[])entry(s.dimensions.model).cost=(entry(s.dimensions.model).cost||0)+s.points.reduce((x,[,v])=>x+v,0);
      for(const s of tokens?.details?.models||[])entry(s.id).tokens+=s.points.reduce((x,[,v])=>x+v,0);
      rows.push(...byModel.values());
    }
    const active=rows.filter(r=>r.cost>0||r.tokens>0).sort((x,y)=>(y.cost||0)-(x.cost||0)||y.tokens-x.tokens),idle=rows.filter(r=>!active.includes(r));
    const max=Math.max(...active.map(r=>r.cost||0),0);
    $("models").hidden=!active.length;
    $("models").innerHTML=`<div><h2>Cost by model</h2><div class="sub">${active.length} of ${rows.length} models active in this range</div></div>
      <div class="bars">${active.map(r=>{const key=`${agents[r.agent]} · ${r.model}`,color=AIUsage.colorFor(key);return `<div class="model"><div class="head"><span class="name"><i class="sw" style="background:${color}"></i>${esc(r.model)} <span class="faint">${agents[r.agent]}</span></span><span class="cost" title="${r.cost==null?"":F.cost(r.cost,true)}">${r.cost==null?"—":F.cost(r.cost)}</span></div>
        <div class="track thick" aria-hidden="true"><span style="width:${max&&r.cost?Math.max(1,r.cost/max*100):0}%;background:${color}"></span></div>
        <div class="meta"><span title="${F.full(r.tokens)}">${F.tokens(r.tokens)} tokens</span>${r.cost&&r.tokens?`<span>${F.cost(r.cost/r.tokens*1e6)} / 1M</span>`:""}${r.agent==="codex"?`<span>${r.cost==null?"no price":"estimated price"}</span>`:""}</div></div>`;}).join("")}
      ${idle.map((r,i)=>`<div class="idle${i?"":" first"}"><span class="name"><i class="sw" style="background:${AIUsage.colorFor(`${agents[r.agent]} · ${r.model}`)}"></i>${esc(r.model)}</span><span class="num">—</span></div>`).join("")}</div>`;
  }
  function renderHeat() {
    const activity=datasets.get("all.activity");if(!activity)return;
    // Cells sum every agent; the tooltip splits each cell back per agent.
    const cells=new Map(),split=new Map();for(const s of activity.series)for(const [t,v]of s.points){const d=new Date(t*1000),key=`${(d.getDay()+6)%7},${d.getHours()}`,parts=split.get(key)||{};cells.set(key,(cells.get(key)||0)+v);parts[s.dimensions.model]=(parts[s.dimensions.model]||0)+v;split.set(key,parts);}
    const data=[];for(let day=0;day<7;day++)for(let hour=0;hour<24;hour++)data.push([hour,day,cells.get(`${day},${hour}`)||0]);
    const days=["Mon","Tue","Wed","Thu","Fri","Sat","Sun"];
    $("heat-chart").closest("section").hidden=!data.some(v=>v[2]>0);
    // The heatmap reads a separate hourly block capped at 30 days (7 at least), so its window is stated.
    $("heat-sub").textContent=`tokens by weekday and hour · local time · last ${Math.round((activity.range.end-activity.range.start)/86400)} days`;
    heatChart=heatChart||AIUsage.init($("heat-chart"));
    heatChart.setOption({grid:{left:4,right:4,top:4,bottom:4,containLabel:true},tooltip:{trigger:"item",formatter:p=>{const parts=Object.entries(split.get(`${p.value[1]},${p.value[0]}`)||{}).filter(([,v])=>v>0);return `${days[p.value[1]]} ${two(p.value[0])}:00 · ${F.full(p.value[2])} tokens${parts.map(([a,v])=>`<br><span style="color:${AIUsage.colorFor(agents[a])}">●</span> ${agents[a]} ${F.full(v)}`).join("")}`;}},
      xAxis:{type:"category",data:Array.from({length:24},(_,h)=>two(h)),axisLine:{show:false},axisLabel:{interval:5}},yAxis:{type:"category",inverse:true,data:days,axisLine:{show:false}},
      visualMap:{show:false,min:0,max:Math.max(1,...data.map(v=>v[2]))},series:[{id:"heat",type:"heatmap",data}]});
  }
  function renderSpikes(summary,range) {
    const items=(datasets.get("all.spikes")?.details||[]).slice().sort((a,b)=>b.factor-a.factor).slice(0,4),weekday=new Intl.DateTimeFormat("en",{weekday:"short"});
    const now=chosen().map(a=>{const tokens=summary[a]?.total?.value,sessions=summary[a]?.sessions;
      return `<div><span><span style="color:${AIUsage.colorFor(agents[a])}">●</span> ${agents[a]}</span><span class="${tokens?"num":"faint"}">${!tokens?"no activity":sessions?.value==null?"sessions not reported":`${F.full(sessions.value)} new sessions`}</span></div>`;}).join("");
    $("spikes").innerHTML=`<div><h2>Usage spikes</h2><div class="sub">days above 2.5× the preceding 7-day average</div></div>
      <div class="spikes">${items.map((v,i)=>{const d=new Date(v.time*1000);return `<div class="spike${i===0&&v.factor>=10?" top":""}"><span class="factor">×${v.factor.toFixed(1)}</span><div><span>${weekday.format(d)}, ${two(d.getDate())}.${two(d.getMonth()+1)}</span><span class="num" title="${F.full(v.tokens)} tokens">${F.tokens(v.tokens)} tokens</span></div></div>`;}).join("")||`<div class="note">No spikes in the daily data for this range.</div>`}</div>
      <div class="agents-now"><div class="eyebrow">Agents in this range</div>${now}</div>`;
  }
  // ---- Agent details: shared comparison, then one panel per agent built from what it reports. ----
  let agentTab="codex";
  try{agentTab=localStorage.getItem("agentTab")||agentTab;}catch(_){}
  const LATENCY=[["firstToken","To first token"],["firstOutput","To first output item"],["turnDuration","Turn duration"],["toolDuration","Tool duration"]];
  const COUNTS=[["active","Active time","split: user / cli"],["commits","Commits","count in range"],["lines","Lines of code","added / removed"],["sessions","New sessions","started in range"],["turns","Turns","conversation turns"],["tools","Tool calls","calls · failed"]];
  const total=points=>(points||[]).reduce((s,[,v])=>s+(v||0),0);
  function miniBars(points,range,color) {
    const first=Math.min(...points.filter(([,v])=>v>0).map(([t])=>t));
    if(!Number.isFinite(first))return "";
    const values=new Map(points),times=buckets(range).filter(t=>t>=first),max=Math.max(...times.map(t=>values.get(t)||0));
    return `<div class="mini" aria-hidden="true">${times.map((t,i)=>`<span title="${timeLabel(t,range)}" style="height:${Math.max(values.get(t)?6:2,Math.round((values.get(t)||0)/max*100))}%;background:${values.get(t)?color:"var(--track)"};opacity:${i===times.length-1?1:.55}"></span>`).join("")}</div>
      <div class="mini-axis" aria-hidden="true"><span>${timeLabel(times[0],range)}</span><span>${times.length>1?timeLabel(times[times.length-1],range):""}</span></div>`;
  }
  const metricCard=(label,value,note,body,title="")=>`<div class="card metric"><div class="eyebrow">${label}</div><div class="figure"><span class="kpi-value" title="${esc(title)}">${value}</span>${note?`<span class="note">${note}</span>`:""}</div>${body}</div>`;
  function workCard(d,range) {
    // Turn time is grouped by local day, or by week/month when the range has too many days.
    const groups=new Map(),days=(range.end-range.start)/86400,unit=days<=14?"day":days<=120?"week":"month";
    for(const [t,v]of d.details.totalTimeline||[]){
      const x=new Date(t*1000);if(unit==="week")x.setDate(x.getDate()-(x.getDay()+6)%7);
      const key=unit==="month"?`${x.getFullYear()}-${two(x.getMonth()+1)}`:`${x.getFullYear()}-${two(x.getMonth()+1)}-${two(x.getDate())}`;
      groups.set(key,(groups.get(key)||0)+v);
    }
    const rows=[...groups].filter(([,v])=>v>0).sort(),max=Math.max(...rows.map(([,v])=>v),0),weekday=new Intl.DateTimeFormat("en",{weekday:"short"});
    const label=key=>{const [y,m,day]=key.split("-");return unit==="month"?`${m}.${y}`:`${unit==="week"?"wk ":weekday.format(new Date(+y,m-1,+day))+" "}${day}.${m}`;};
    return `<div class="card"><div class="card-top"><div><h3>Agent working time</h3><span class="sub">Σ turn duration, including tools and waiting · by ${unit}</span></div><span class="kpi-value" style="font-size:28px" title="${F.full(Math.round(d.details.total_seconds))} s">${F.duration(d.details.total_seconds)}</span></div>
      <div class="bars">${rows.map(([key,v])=>`<div class="work"><span>${label(key)}</span><div class="track" aria-hidden="true"><span style="width:${Math.max(1,v/max*100)}%;background:var(--codex)"></span></div><span class="num">${F.duration(v)}</span></div>`).join("")||`<div class="note">No completed turns in this range.</div>`}</div></div>`;
  }
  function toolsCard(d,color) {
    const ranking=d.details.ranking||[],calls=ranking.reduce((s,r)=>s+r.calls,0),failed=ranking.reduce((s,r)=>s+r.failed,0),max=ranking[0]?.calls||0;
    const row=r=>`<div class="tool"><span title="${esc(r.tool)}">${esc(r.tool)}</span><div class="track" aria-hidden="true"><span style="width:${(r.calls-r.failed)/max*100}%;background:${color}"></span><span style="width:${r.failed?Math.max(1.2,r.failed/max*100):0}%;background:var(--warn)"></span></div><span class="num" title="${F.full(r.failed)} failed">${F.full(r.calls)}</span></div>`;
    return `<div class="card"><div class="card-top"><div><h3>Tool calls</h3><span class="sub">${F.full(calls)} calls · ranking by tool</span></div>${calls?`<span class="badge"><b class="num">${F.full(failed)}</b> failed · ${F.pct(failed/calls*100,2)}</span>`:""}</div>
      <div class="bars" style="gap:10px">${ranking.slice(0,10).map(row).join("")}</div>
      ${ranking.length>10?`<details class="more-tools"><summary class="note">${ranking.length-10} more ${ranking.length===11?"tool":"tools"}</summary><div class="bars" style="gap:10px">${ranking.slice(10).map(row).join("")}</div></details>`:""}${failed?`<div class="note faint">the yellow part of a bar is that tool's failed calls</div>`:""}</div>`;
  }
  /** Cards for every measurement this agent reports; returns placeholders for the empty state too. */
  function agentPanel(agent,summary,range) {
    const color=AIUsage.colorFor(agents[agent]),small=[],wide=[],placeholders=[];let activity=summary[agent]?.total?.value>0;
    // Histogram quantiles when the agent exports buckets; the Σsum/Σcount mean otherwise.
    const at=(d,q)=>`${d.details[q+"Capped"]?"≥ ":""}${F.duration(d.details[q])}`;
    for(const [m,label]of LATENCY){const d=reported(agent,m);if(!d)continue;placeholders.push([label,"median and p95"]);
      if(d.details.p50!=null){activity=true;small.push(metricCard(label,at(d,"p50"),"p50",`<div class="note">p95 ${at(d,"p95")} · mean ${F.duration(d.total)}</div>`+miniBars(d.details.p50Timeline||[],range,color),`${F.full(Math.round(d.details.p50*1000))} ms`));}
      else if(d.total!=null){activity=true;small.push(metricCard(label,F.duration(d.total),"average",miniBars(d.series[0]?.points||[],range,color),`${F.full(Math.round(d.total*1000))} ms`));}}
    for(const [m,label,hint]of COUNTS){const d=reported(agent,m);if(!d)continue;placeholders.push([label,hint]);
      const sum=total(d.details?.timeline);if(!sum)continue;activity=true;
      const kinds=d.series.map(s=>[s.dimensions.kind,total(s.points)]).filter(([,v])=>v>0);
      const value=m==="active"?F.duration(sum):m==="lines"?`+${F.tokens(summary[agent]?.added?.value||0)} / −${F.tokens(summary[agent]?.removed?.value||0)}`:F.full(sum);
      const failed=m==="tools"?total(d.details.failures):0;
      const note=m==="active"&&kinds.length?kinds.map(([k,v])=>`${esc(k)} ${F.duration(v)}`).join(" · "):m==="lines"?"added / removed":m==="tools"?`${F.full(failed)} failed`:"";
      small.push(metricCard(label,value,note,miniBars(d.details.timeline,range,color),m==="active"?`${F.full(Math.round(sum))} s`:F.full(sum)));}
    const turn=reported(agent,"turnDuration");if(turn&&turn.details.total_seconds)wide.push(workCard(turn,range));
    const tools=reported(agent,"tools");if(tools?.details.ranking?.length){activity=true;wide.push(toolsCard(tools,color));}
    return {activity,count:placeholders.length,html:small.length||wide.length?`${small.length?`<div class="kpi-grid metrics">${small.join("")}</div>`:""}${wide.length?`<div class="row-2">${wide.join("")}</div>`:""}`:"",placeholders};
  }
  function renderAgents(summary,range) {
    const sel=chosen(),both=sel.length>1;
    if(!sel.includes(agentTab))agentTab=sel[0];
    const panels=Object.fromEntries(sel.map(a=>[a,agentPanel(a,summary,range)])),agent=both?agentTab:sel[0],panel=panels[agent];
    const rows=[["Cost","cost",v=>F.cost(v),v=>F.cost(v,true)],["Tokens","total",F.tokens,F.full],["Cache hit","cacheHit",v=>F.pct(v*100),v=>F.pct(v*100,3)],["Output","output",F.tokens,F.full],["Reasoning","reasoning",F.tokens,F.full],["New sessions","sessions",F.full,F.full]];
    const compare=both?`<section class="card" aria-label="Agent comparison"><div class="card-top"><h2>Comparison · shared metrics</h2><div class="legend">${sel.map(a=>`<span><span style="color:${AIUsage.colorFor(agents[a])}">●</span> ${agents[a]}</span>`).join("")}</div></div>
      <div class="compare-rows" style="--agents:${sel.length}">${rows.map(([label,key,short,full])=>{const values=sel.map(a=>summary[a]?.[key]),max=Math.max(...values.map(v=>v?.value||0));
        return `<div class="compare-row"><span>${label}</span>${sel.map((a,i)=>{const v=values[i],missing=v?.state==="unavailable"||v==null;const text=v?.value!=null?short(v.value):missing?"not reported":key==="cacheHit"?"—":short(0);
          const width=v?.value?key==="cacheHit"?v.value*100:v.value/max*100:0;
          return `<div class="cell"><div class="track" aria-hidden="true"><span style="width:${width}%;background:${AIUsage.colorFor(agents[a])}"></span></div><span class="num${v?.value==null?" missing":""}" title="${v?.value!=null?full(v.value):""}">${text}</span></div>`;}).join("")}</div>`;}).join("")}</div></section>`:"";
    const lists=[...datasets.values()].filter(d=>d.agent===agent&&d.state!=="unavailable").map(d=>d.details?.timeline||d.details?.categories?.find(c=>c.id==="total")?.points||[]);
    const first=Math.min(...lists.flat().filter(([,v])=>v>0).map(([t])=>t)),days=Math.ceil((range.end-first)/86400);
    const chip=!panel.activity?"no data in this range":Number.isFinite(first)&&first>range.start?`data since ${timeLabel(first,range)}${range.end-range.start>86400+range.step?` · axis trimmed to ${days} ${days===1?"day":"days"}`:""}`:"";
    const tabs=both?`<div class="tabs" role="tablist" aria-label="Agent">${sel.map(a=>`<button type="button" role="tab" data-tab="${a}" aria-selected="${a===agent}" style="--tab-color:${AIUsage.colorFor(agents[a])}"><i style="background:${AIUsage.colorFor(agents[a])}"></i>${agents[a]} <span class="count">${panels[a].count} metrics</span></button>`).join("")}</div>`
      :`<h2 class="agent-title"><i style="background:${AIUsage.colorFor(agents[agent])}"></i>${agents[agent]} · agent metrics</h2>`;
    const longer=["90d","180d","1y","all"].includes(selection.range);
    const body=panel.activity&&panel.html?panel.html:`<div class="card empty"><div><h3>No ${agents[agent]} activity in this range</h3><p>${panel.placeholders.length?"When there is activity, these metrics appear here:":`${agents[agent]} reports no agent-specific metrics.`}</p></div>
      <div class="placeholders">${panel.placeholders.map(([label,hint])=>`<div><span class="eyebrow">${label}</span><span class="num">—</span><span>${hint}</span></div>`).join("")}</div>${longer?"":`<div><button type="button" class="link" data-goto="90d">Show the last 90 days</button></div>`}</div>`;
    const latency=LATENCY.map(([m])=>reported(agent,m)).filter(d=>d?.total!=null),quantiles=latency.some(d=>d.details.p50!=null);
    $("agents").innerHTML=`${compare}<div class="agent-bar" role="${both?"":"group"}">${tabs}${chip?`<span class="chip">${chip}</span>`:""}</div><div ${both?`role="tabpanel"`:""} class="agents">${body}</div>${latency.length&&panel.activity?`<p class="note faint" style="margin:0">${quantiles?"Durations are p50 with p95 below, approximated from the histogram buckets; ≥ marks a value in the open top bucket. Bars show p50 per bucket.":"Durations are averages (Σ sum / Σ count) of the duration histograms."}</p>`:""}`;
  }
  async function copyCharts(button,specs) {
    const id=snapshot,chosen=structuredClone(specs),scope={...selection},label=button.textContent;
    if(!id){button.textContent="Waiting for snapshot";return;}
    try {
      const response=await fetch(`/api/dashboard/export?${new URLSearchParams({snapshot:id})}`),source=await response.json();if(!response.ok)throw Error(source.error||"Export failed");
      const lines=chosen.map(chart=>JSON.stringify({chart:chart.title,unit:chart.unit,snapshot:id,selection:scope,time_boundaries:source.datasets.find(d=>d.id===chart.series[0]?.dataset)?.range,series:chart.series.map(s=>({name:s.label||s.id,values:s.points})),queries:source.queries,warnings:source.warnings}));
      await navigator.clipboard.writeText(lines.length===1?JSON.stringify(JSON.parse(lines[0]),null,2):lines.join("\n")+"\n");button.textContent="Copied";
    }catch(error){button.textContent="Copy failed";button.title=error.message;}
    setTimeout(()=>{button.textContent=label;},2200);
  }
  window.usage={initial,select,apply,restore(){theme();},debug(){return {epoch,snapshot,sourceAt,datasets,widgets};}};
  document.addEventListener("DOMContentLoaded",()=>{
    theme();$("theme").onclick=()=>{currentTheme=currentTheme==="dark"?"light":"dark";try{localStorage.setItem("theme",currentTheme);}catch(_){}theme();};
    $("copy-all").onclick=()=>copyCharts($("copy-all"),[...widgets.values()].map(w=>w.spec));
    for(const b of document.querySelectorAll("[data-metric]"))b.onclick=()=>{metric=b.dataset.metric;try{localStorage.setItem("metric",metric);}catch(_){}if(datasets.size)renderOverview();};
    $("agents").onclick=event=>{
      const tab=event.target.closest("[data-tab]"),goto=event.target.closest("[data-goto]");
      if(tab){agentTab=tab.dataset.tab;try{localStorage.setItem("agentTab",agentTab);}catch(_){}renderOverview();}
      if(goto)$("range-"+goto.dataset.goto).click();
    };
    $("raw-toggle").onclick=()=>{const show=$("raw").hidden;$("raw").hidden=!show;$("raw-toggle").setAttribute("aria-expanded",String(show));$("raw-toggle").textContent=`${show?"Hide":"Show"} raw charts (${widgets.size})`;if(show)$("raw").scrollIntoView({behavior:"smooth"});};
    setInterval(()=>{if(lastPacket&&Date.now()-lastPacket>75000){$("updated").classList.add("stale");$("error").hidden=false;$("error").textContent="Live connection interrupted. Reconnecting…";}},15000);
  });
  window.addEventListener("pagehide",()=>widgets.forEach(w=>w.chart?.dispose()));
})();
