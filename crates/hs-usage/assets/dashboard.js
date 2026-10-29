/* ECharts presentation bridge. Accounting and query planning belong to Rust. */
"use strict";
(() => {
  const agents = {claude:"Claude Code", codex:"Codex"};
  const widgets = new Map(), datasets = new Map();
  let selection, epoch = 0, snapshot, compare = false, lastPacket = 0, sourceAt = 0;
  let currentTheme = matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  try { currentTheme = localStorage.getItem("theme") || currentTheme; } catch (_) {}
  const el = (tag, cls, text) => { const e=document.createElement(tag); if(cls)e.className=cls; if(text!==undefined)e.textContent=text;return e; };
  const $ = id => document.getElementById(id);
  const heatmapScale = new Intl.NumberFormat("en",{notation:"compact",maximumFractionDigits:1});
  const number = v => v == null ? "—" : new Intl.NumberFormat(undefined,{maximumFractionDigits:2}).format(v);
  const format = (v,unit) => v == null ? "—" : unit === "USD" ? new Intl.NumberFormat(undefined,{style:"currency",currency:"USD",maximumFractionDigits:4}).format(v) : unit === "ratio" ? new Intl.NumberFormat(undefined,{style:"percent",maximumFractionDigits:1}).format(v) : number(v);
  function initial() {
    const out={agent:"all",host:"*",project:"*",range:"24h",_compare:false,_more:false,_sync:0,_packet:""};
    try {for(const key of ["agent","host","project","range"]) out[key]=localStorage.getItem(key)||out[key];out._compare=localStorage.getItem("comparison")==="true";}catch(_){}
    if(!["all","claude","codex"].includes(out.agent))out.agent="all";
    if(!Object.keys(AIUsage.RANGES).includes(out.range))out.range="24h";
    if(out._compare)out.agent="all";
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
      scopeOptions("host",full.hosts,"All servers");scopeOptions("project",full.projects,"All projects");
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
      scopeOptions("host",packet.hosts,"All servers");scopeOptions("project",packet.projects,"All projects");
    }
    $("error").hidden=true;
    render(dirty);
  }
  function resync() {$("error").hidden=false;$("error").textContent="Synchronizing dataset versions…";$("resync").click();}
  function freshness(f) {sourceAt=f.source_at;$("updated").textContent=`Updated ${new Date(f.source_at*1000).toLocaleTimeString()}`;}
  const missingLabel = value => value?.state==="unavailable" ? "Not reported" : value?.state==="partial" ? "Partial data" : "No measurements";
  function mode(value) {
    compare=value;$("comparison").hidden=!value;$("overview").hidden=$("cards").hidden=value;
    try{localStorage.setItem("comparison",String(value));}catch(_){}
    if(datasets.size)render(new Set(datasets.keys()));
    if(!value)requestAnimationFrame(()=>widgets.forEach(w=>w.chart?.resize()));
  }
  function theme() {
    document.documentElement.dataset.theme=currentTheme;
    $("theme").textContent=currentTheme==="dark"?"☀ Light":"☾ Dark";
    // Series colors are resolved from CSS tokens, so charts are redrawn after the theme rebuild.
    AIUsage.refresh();
    for(const w of widgets.values())if(w.spec)draw(w);
  }
  function definitions() {
    const list=[], selected=Object.keys(agents).filter(a=>selection.agent==="all"||selection.agent===a);
    const parts=(measurement,projector,shared=true)=>selected.flatMap(agent=>{
      const d=datasets.get(`${agent}.${measurement}`);
      if(!d||d.state==="unavailable")return [];
      return projector(d).map(s=>({...s,label:s.label||agents[agent],agent,dataset:d.id}));
    });
    const supported=(measurement,shared=true)=>!shared||selected.every(a=>datasets.get(`${a}.${measurement}`)?.state!=="unavailable");
    // Series without a single nonzero value are dropped, and a panel with no series is not shown.
    const add=(id,title,unit,data,options={})=>{data=data.filter(s=>AIUsage.nonEmpty([{data:s.points}]).length);if(data.length)list.push({id,title,unit,series:data,sourceIds:[...new Set(data.map(s=>s.dataset))],...options});};
    const timeline=d=>[{id:d.agent,label:agents[d.agent]+(d.agent==="codex"&&d.measurement==="cost"?" (estimated)":""),points:d.details?.timeline||[]}];
    add("cost","Cost over time","USD",parts("cost",timeline));
    add("models-cost","Cost by model","USD",parts("cost",d=>d.series.map(s=>({...s,label:agents[d.agent]+" · "+s.dimensions.model}))),{stack:true});
    for(const [id,title]of [["total","Tokens over time"],["input","Uncached input tokens"],["cached","Cached input tokens"],["written","Cache write tokens"],["output","Output tokens (includes reasoning)"],["reasoning","Reasoning tokens (subset of output)"]]) {
      add(id,title,"tokens",parts("tokens",d=>(d.details.categories||[]).filter(s=>s.id===id).map(s=>({...s,id:d.agent,label:agents[d.agent]}))));
    }
    add("models-tokens","Tokens by model","tokens",parts("tokens",d=>(d.details.models||[]).map(s=>({...s,label:agents[d.agent]+" · "+s.label}))),{stack:true});
    add("cacheHit","Cache hit","ratio",parts("tokens",d=>[{id:d.agent,label:agents[d.agent],points:d.details.cacheHit||[]}]),{max:1});
    for(const [measurement,title,unit]of [["sessions","New sessions","count"],["active","Active time","seconds"],["commits","Commits","count"],["lines","Lines of code","count"],["turns","Turns","count"],["tools","Tool calls","count"],["turnDuration","Average turn duration","seconds"],["firstToken","Time to first token","seconds"],["firstOutput","Time to first output item","seconds"],["toolDuration","Average tool duration","seconds"]]) {
      if(!supported(measurement))continue;
      add(measurement,title,unit,parts(measurement,measurement==="lines"?d=>d.series.map(s=>({...s,label:agents[d.agent]+" · "+s.dimensions.kind})):timeline));
    }
    if(selected.length===1) {
      if(supported("active"))add("active-kind","Active time by kind","seconds",parts("active",d=>d.series));
      if(supported("tools")){
        add("tool-failures","Failed tool calls","count",parts("tools",d=>[{id:d.agent,points:d.details.failures||[]}]));
        add("tools-kind","Tool calls by tool","count",parts("tools",d=>d.details.tools||[]));
      }
      const duration=datasets.get(`${selected[0]}.turnDuration`);
      if(duration?.details.total_seconds!=null)add("turn-time","Total turn duration","seconds",[{id:"total",label:"Total turn duration",dataset:duration.id,points:duration.details.totalTimeline||[]}],{note:`Total: ${number(duration.details.total_seconds)} s`});
    }
    const activity=datasets.get("all.activity");
    // The heatmap reads a separate hourly block capped at 30 days (7 at least), so its window is stated.
    if(activity)add("activity","Activity by local weekday/hour","tokens",activity.series.map(s=>({...s,dataset:activity.id})),{heat:true,note:`last ${Math.round((activity.range.end-activity.range.start)/86400)} days`});
    return list;
  }
  function render(dirty) {
    if(dirty.has("all.summary"))renderSummary();
    const definitionsNow=definitions(),ids=new Set(definitionsNow.map(s=>s.id));
    for(const [id,w]of widgets)if(!ids.has(id)){w.chart?.dispose();w.card.remove();widgets.delete(id);}
    for(const spec of definitionsNow) {
      let w=widgets.get(spec.id);
      if(!w){w=create(spec);widgets.set(spec.id,w);w.spec=spec;draw(w);}
      else if([...w.spec.sourceIds,...spec.sourceIds].some(id=>dirty.has(id))){w.spec=spec;draw(w);}
    }
    if(dirty.has("all.spikes"))renderSpikes();
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
      option={tooltip:{position:"top"},grid:{left:12,right:16,top:16,bottom:74,containLabel:true},xAxis:{type:"category",axisLabel:{interval:"auto",hideOverlap:true,margin:10},axisTick:{alignWithLabel:true},data:Array.from({length:24},(_,h)=>String(h).padStart(2,"0"))},yAxis:{type:"category",axisLabel:{interval:0,margin:8},data:["Mon","Tue","Wed","Thu","Fri","Sat","Sun"]},visualMap:{min:0,max:Math.max(1,...data.map(v=>v[2])),calculable:true,orient:"horizontal",left:"center",bottom:20,itemWidth:12,itemHeight:120,padding:8,formatter:value=>heatmapScale.format(value).replace(/K$/,"k")},series:[{id:"heat",type:"heatmap",data,emphasis:{itemStyle:{borderWidth:1}}}]};
    } else {
      option={grid:{left:65,right:18,top:55,bottom:55},legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0},tooltip:{trigger:"axis",axisPointer:{type:"shadow"},valueFormatter:v=>format(v,spec.unit)},xAxis:{type:"time",min:timeRange?((since??timeRange.start)-timeRange.step/2)*1000:undefined,max:timeRange?timeRange.end*1000:undefined,axisLabel:{hideOverlap:true},axisPointer:{label:{formatter:p=>new Date(p.value).toLocaleString()}}},yAxis:{type:"value",max:spec.max,minInterval:["count","tokens"].includes(spec.unit)?1:undefined,axisLabel:{formatter:v=>spec.unit==="ratio"?`${v*100}%`:number(v)}},dataZoom:[{type:"inside"}],series:spec.series.map(s=>({id:`${s.dataset}:${s.id}`,name:s.label||s.id,type:spec.unit==="ratio"?"line":"bar",stack:spec.stack?"total":undefined,itemStyle:{color:seriesColor(s)},lineStyle:{color:seriesColor(s)},data:s.points.map(([t,v])=>[t*1000,v]),connectNulls:false}))};
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
  function renderSummary() {
    const summary=datasets.get("all.summary")?.details||{},active=Object.keys(agents).filter(a=>selection.agent==="all"||selection.agent===a);
    const tiles=[];
    for(const agent of active)for(const [key,label,unit]of [["cost","cost","USD"],["total","tokens","tokens"],["sessions","new sessions","count"],["cacheHit","cache hit","ratio"]]) {
      const value=summary[agent]?.[key];const tile=el("div","tile");tile.append(el("div","label",`${agents[agent]} ${agent==="codex"&&key==="cost"?"estimated ":""}${label}`),el("div","value",format(value?.value,unit)));
      tile.append(el("div","note",value?.value==null?missingLabel(value):value?.state==="partial"?"Partial data":""));tiles.push(tile);
    }
    $("tiles").replaceChildren(...tiles);$("summary").hidden=selection.agent!=="all";$("overview").classList.toggle("has-summary",selection.agent==="all");
    const totals=[];for(const [key,title,unit]of [["cost","Total cost","USD"],["total","Total tokens","tokens"]]){const available=active.filter(a=>summary[a]?.[key]?.value!=null),missing=active.filter(a=>!available.includes(a)),tile=el("div","tile");
      // Totals are supplied below by the backend summary; no rounded accounting in the browser.
      const value=summary.totals?.[key];tile.append(el("div","label",title),el("div","value",format(value?.value,unit)),el("div","note",missing.length?"Partial total · unavailable: "+missing.map(a=>agents[a]).join(", "):"All agents"));totals.push(tile);}
    $("summary").replaceChildren(...totals);
    const table=el("table"),caption=el("caption","","Agent comparison"),head=el("tr");head.append(el("th","","Metric"),...active.map(a=>el("th","",agents[a])));table.append(caption,head);
    for(const [key,title,unit,token]of [["cost","Cost","USD"],["total","Total tokens","tokens",true],["input","Uncached input tokens","tokens",true],["cached","Cached input tokens","tokens",true],["written","Cache write tokens","tokens",true],["output","Output (includes reasoning)","tokens",true],["reasoning","Reasoning (subset of output)","tokens",true],["cacheHit","Cache hit","ratio",true],["sessions","New sessions","count"],["active","Active time","seconds"],["commits","Commits","count"],["added","Lines added","count"],["removed","Lines removed","count"],["turns","Turns","count"],["turnDuration","Average turn duration","seconds"],["firstToken","Time to first token","seconds"],["firstOutput","Time to first output","seconds"],["tools","Tool calls","count"],["toolDuration","Average tool duration","seconds"]]){
      if(!token&&!active.every(a=>summary[a]?.[key]&&summary[a][key].state!=="unavailable"))continue;
      const row=el("tr",["cost","total"].includes(key)?"primary":"");row.append(el("th","",title));for(const agent of active){const v=summary[agent]?.[key],cell=el("td","",format(v?.value,unit));if(v?.value==null)cell.append(el("span","note",v?.state==="unavailable"?"Not reported by this agent":"No data for selection"));if(v?.state==="partial")cell.append(el("span","note","Partial data"));row.append(cell);}table.append(row);
    }
    $("comparison").replaceChildren(table);
  }
  function renderSpikes() {
    let card=$("spikes");if(!card){card=el("article","card");card.id="spikes";$("cards").append(card);}
    const items=datasets.get("all.spikes")?.details||[];
    card.replaceChildren(el("h2","","Usage spikes · above 2.5× preceding 7 days"),...items.slice(-6).reverse().map(v=>el("p","",`${new Date(v.time*1000).toLocaleDateString()}: ${number(v.tokens)} tokens · ${number(v.factor)}×`)));
    if(!items.length)card.append(el("p","note","No spikes detected in the available daily data."));
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
  window.usage={initial,select,apply,mode,restore(){theme();},debug(){return {epoch,snapshot,sourceAt,datasets,widgets};}};
  document.addEventListener("DOMContentLoaded",()=>{
    theme();$("theme").onclick=()=>{currentTheme=currentTheme==="dark"?"light":"dark";try{localStorage.setItem("theme",currentTheme);}catch(_){}theme();};
    $("copy-all").onclick=()=>copyCharts($("copy-all"),[...widgets.values()].map(w=>w.spec));
    setInterval(()=>{if(lastPacket&&Date.now()-lastPacket>75000){$("error").hidden=false;$("error").textContent="Live connection interrupted. Reconnecting…";}},15000);
  });
  window.addEventListener("pagehide",()=>widgets.forEach(w=>w.chart?.dispose()));
})();
