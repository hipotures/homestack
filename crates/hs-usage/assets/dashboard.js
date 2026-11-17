/* ECharts presentation bridge. Accounting and query planning belong to Rust. */
"use strict";
(() => {
  const version = document.querySelector('meta[name="ai-usage-version"]').content;
  let reloading = false;
  const agents = {claude:"Claude Code", codex:"Codex", antigravity:"Antigravity"};
  const widgets = new Map(), datasets = new Map();
  let selection, epoch = 0, snapshot, lastPacket = 0, sourceAt = 0;
  const el = (tag, cls, text) => { const e=document.createElement(tag); if(cls)e.className=cls; if(text!==undefined)e.textContent=text;return e; };
  const $ = id => document.getElementById(id);
  // ---- Toasts (The Mechanics): problems are notifications, not page text. ----
  // A condition toast (lost connection, failed load) stays while the problem lasts and leaves by itself when it
  // clears; ✕ hides it until the problem occurs again. Action results are toasts that leave after ~4 s.
  const TOAST_ICON={success:"M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5 8l2 2 4-4",warning:"M8 1.5l6.5 12h-13zM8 6v3.5M8 11.5v.01",danger:"M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM5.5 5.5l5 5M10.5 5.5l-5 5",info:"M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM8 7v4.5M8 4.5v.01"};
  const CLOSE_ICON='<svg class="tm-icon tm-icon--sm" viewBox="0 0 16 16" aria-hidden="true"><path d="M4 4l8 8M12 4l-8 8"/></svg>';
  function toast({tone,title,body,timeout=4000,onClose}){
    const box=el("div",`tm-alert tm-alert--${tone} tm-toast`);box.setAttribute("role",tone==="danger"?"alert":"status");
    box.insertAdjacentHTML("afterbegin",`<svg class="tm-icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false"><path d="${TOAST_ICON[tone]}"/></svg>`);
    const text=el("div"),bodyEl=el("div","tm-alert__body",body||"");text.append(el("div","tm-alert__title",title),bodyEl);
    const close=el("button","tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm");close.type="button";close.setAttribute("aria-label",`Dismiss: ${title}`);close.innerHTML=CLOSE_ICON;
    const remove=()=>{box.remove();onClose?.();};close.onclick=remove;box.append(text,close);
    if(timeout){
      const bar=el("div","tm-toast__progress");bar.style.setProperty("--tm-toast-ms",`${timeout}ms`);box.append(bar);
      let left=timeout,started=Date.now(),timer=setTimeout(remove,left);
      box.addEventListener("mouseenter",()=>{clearTimeout(timer);left-=Date.now()-started;bar.style.animationPlayState="paused";});
      box.addEventListener("mouseleave",()=>{started=Date.now();timer=setTimeout(remove,Math.max(left,1500));bar.style.animationPlayState="running";});
    }
    $("toasts").append(box);
    return {box,setBody:t=>{bodyEl.textContent=t;},remove:()=>{box.remove();}};
  }
  const conditions=new Map(),dismissed=new Set();
  function condition(key,tone,title,body){
    const shown=conditions.get(key);
    if(shown){shown.setBody(body);return;}
    if(dismissed.has(key))return;
    conditions.set(key,toast({tone,title,body,timeout:0,onClose:()=>{conditions.delete(key);dismissed.add(key);}}));
  }
  function resolved(key){dismissed.delete(key);conditions.get(key)?.remove();conditions.delete(key);}
  // Tooltips and tables show full precision; cards use the short AIUsage.fmt forms.
  const format = (v,unit) => v == null ? "—" : unit === "USD" ? AIUsage.fmt.cost(v,true) : unit === "ratio" ? AIUsage.fmt.pct(v*100,2) : unit === "seconds" ? `${AIUsage.fmt.full(Math.round(v*1000)/1000)} s` : AIUsage.fmt.full(Math.round(v*100)/100);
  function initial() {
    const out={agent:"all",host:"*",project:"*",range:"24h",_more:false,_sync:0,_packet:"",_from:"",_to:""};
    try {for(const key of ["agent","host","project","range"]) out[key]=localStorage.getItem(key)||out[key];}catch(_){}
    if(!["all","claude","codex","antigravity"].includes(out.agent))out.agent="all";
    const span=CUSTOM.exec(out.range);
    if(!span&&!Object.keys(AIUsage.RANGES).includes(out.range))out.range="24h";
    // The date fields show the remembered custom span, or the last seven local days.
    const now=Date.now()/1000;
    [out._from,out._to]=span?[localDay(+span[1]),localDay(+span[2]-1)]:[localDay(now-6*86400),localDay(now)];
    $("range-from").max=$("range-to").max=localDay(now);
    // Scope options must exist before Datastar binds a remembered selection.
    for(const key of ["host","project"])if(out[key]!=="*"){const o=el("option","",out[key]);o.value=out[key];$(key).append(o);}
    return out;
  }
  const CUSTOM=/^custom:(\d+):(\d+)$/,pad=n=>String(n).padStart(2,"0");
  const localDay=t=>{const d=new Date(t*1000);return `${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())}`;};
  /** Inclusive local days to the server's half-open `custom:START:END`, or "" when invalid. */
  function custom(from,to) {
    if(!/^\d{4}-\d\d-\d\d$/.test(from)||!/^\d{4}-\d\d-\d\d$/.test(to))return "";
    const start=new Date(from+"T00:00"),end=new Date(to+"T00:00");end.setDate(end.getDate()+1);
    return end>start&&end-start<=600*86400e3?`custom:${start/1000}:${end/1000}`:"";
  }
  function rangeLabel(range) {
    const span=CUSTOM.exec(range),day=t=>{const d=new Date(t*1000);return `${pad(d.getDate())}.${pad(d.getMonth()+1)}`;};
    return span?`${day(+span[1])}–${day(+span[2]-1)}`:({"180d":"180 days","1y":"1 year","all":"All"}[range]||"More");
  }
  function select(next,sync) {
    epoch++; brush=null; selection={...next}; snapshot=undefined;
    try {for(const [key,value] of Object.entries(next))localStorage.setItem(key,value);}catch(_){}
    return epoch;
  }
  function scopeOptions(id,values,label) {
    const target=$(id);
    // Projects follow the selected server: a project absent there falls back to all projects.
    if(id==="project"&&selection.project!=="*"&&selection.host!=="*"&&values.length&&!values.includes(selection.project)){
      target.value="*";target.dispatchEvent(new Event("change",{bubbles:true}));
      return;
    }
    const current=selection[id],wanted=["*",...new Set([...values,current].filter(v=>v!=="*"))];
    if(JSON.stringify([...target.options].map(o=>o.value))===JSON.stringify(wanted))return;
    target.replaceChildren(...wanted.map(value=>{const option=el("option","",value==="*"?label:value);option.value=value;option.selected=value===current;return option;}));
    fitScopes();
  }
  // Scope lists fit the chosen value (CSS field-sizing); browsers without it get the same width from here.
  const measure=document.createElement("canvas").getContext("2d");
  function fitScopes(){
    if(CSS.supports("field-sizing","content"))return;
    for(const id of ["agent","host","project"]){
      const sel=$(id),style=getComputedStyle(sel),text=sel.selectedOptions[0]?.textContent||"";
      measure.font=`${style.fontWeight} ${style.fontSize} ${style.fontFamily}`;
      const edges=["paddingLeft","paddingRight","borderLeftWidth","borderRightWidth"].reduce((n,k)=>n+parseFloat(style[k]),0);
      sel.style.width=`${Math.ceil(measure.measureText(text).width+edges)}px`;
    }
  }
  function apply(packet) {
    if(packet.kind==="version") {
      if(packet.version!==version && !reloading) {reloading=true;window.location.reload();}
      return;
    }
    if(reloading)return;
    if(packet.epoch!==epoch)return;
    lastPacket=Date.now();
    resolved("connection");
    if(packet.kind==="error") {condition("data","danger","Cannot load data",packet.message);return;}
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
    resolved("data");resolved("sync");
    render(dirty);
  }
  function resync() {condition("sync","info","Synchronizing","Synchronizing dataset versions…");$("resync").click();}
  function freshness(f) {sourceAt=f.source_at;const d=new Date(f.source_at*1000);$("updated-text").textContent=`live · ${String(d.getHours()).padStart(2,"0")}:${String(d.getMinutes()).padStart(2,"0")}`;$("updated-mark").dataset.state="complete";}
  // Three themes, one quiet icon button that cycles Light → Dark → Coffee; the icon shows the current theme.
  const THEMES=["light","dark","coffee"],THEME_INFO={
    light:{label:"Light",icon:"M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM8 1v1.5M8 13.5V15M1 8h1.5M13.5 8H15M3 3l1 1M12 12l1 1M3 13l1-1M12 4l1-1"},
    dark:{label:"Dark",icon:"M13.5 9.5A6 6 0 0 1 6.5 2.5a6 6 0 1 0 7 7z"},
    coffee:{label:"Coffee",icon:"M2.5 5.5h9v4a4 4 0 0 1-4 4h-1a4 4 0 0 1-4-4zM11.5 6.5h1.5a1.5 1.5 0 0 1 0 3h-1.5M5 1.5v2M8 1.5v2"}};
  function theme(id) {
    const root=document.documentElement;
    if(id){
      root.classList.add("tm-theme-switching");root.dataset.tmTheme=id;
      requestAnimationFrame(()=>requestAnimationFrame(()=>root.classList.remove("tm-theme-switching")));
      try{localStorage.setItem("tm-theme",id);}catch(_){}
    }
    const current=THEMES.includes(root.dataset.tmTheme)?root.dataset.tmTheme:"light",next=THEMES[(THEMES.indexOf(current)+1)%THEMES.length];
    $("theme-icon").setAttribute("d",THEME_INFO[current].icon);
    const text=`Theme: ${THEME_INFO[current].label} · click for ${THEME_INFO[next].label}`;
    $("theme-tip").textContent=text;$("theme").setAttribute("aria-label",text);
    // Series colors are resolved from CSS tokens, so charts are redrawn after the theme rebuild.
    AIUsage.refresh();
    for(const w of widgets.values())if(w.spec)draw(w);
    if(datasets.size){renderOverview();renderModelViews();}
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
    const timeline=d=>[{id:d.agent,label:agents[d.agent]+(d.details?.estimated&&d.measurement==="cost"?" (estimated)":""),points:d.details?.timeline||[]}];
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
    // Per-model views read only token datasets, so other dataset updates leave them untouched.
    if([...dirty].some(id=>id.endsWith(".tokens")||id.endsWith(".effort")))renderModelViews();
    const definitionsNow=definitions(),ids=new Set(definitionsNow.map(s=>s.id));
    for(const [id,w]of widgets)if(!ids.has(id)){w.chart?.dispose();w.card.remove();widgets.delete(id);}
    for(const spec of definitionsNow) {
      let w=widgets.get(spec.id);
      if(!w){w=create(spec);widgets.set(spec.id,w);w.spec=spec;draw(w);}
      else if([...w.spec.sourceIds,...spec.sourceIds].some(id=>dirty.has(id))){w.spec=spec;draw(w);}
    }
    $("raw-count").textContent=String(widgets.size);
  }
  const COPY_ICON='<svg class="tm-icon tm-icon--sm" viewBox="0 0 16 16" aria-hidden="true"><path d="M5.5 5.5h8v8h-8zM2.5 10.5v-8h8"/></svg>';
  function create(spec) {
    const card=el("article","tm-panel"),head=el("header","tm-panel__head tm-panel__head--rich"),heading=el("div","tm-panel__heading"),actions=el("div","tm-panel__actions"),body=el("div","tm-panel__body");
    const toggle=el("button","tm-btn tm-btn--outline tm-btn--sm tm-btn--toggle","Table"),copy=el("button","tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm"),note=el("div","tm-chart-readout aiu-readout"),chartHost=el("div","tm-chart aiu-chart"),tableHost=el("div","tm-table-wrap aiu-table");
    card.id="widget-"+spec.id;chartHost.id="chart-"+spec.id;chartHost.setAttribute("data-ignore-morph","");chartHost.setAttribute("role","img");chartHost.setAttribute("aria-label",spec.title);
    copy.type=toggle.type="button";copy.hidden=true;copy.innerHTML=COPY_ICON;copy.setAttribute("aria-label","Copy data as JSON");toggle.setAttribute("aria-pressed","false");
    tableHost.tabIndex=0;tableHost.setAttribute("role","region");tableHost.setAttribute("aria-label",spec.title+" data");
    heading.append(el("h3","tm-panel__title",spec.title),el("span","tm-panel__subtitle",spec.unit));actions.append(copy,toggle);head.append(heading,actions);
    body.append(note,chartHost,tableHost);card.append(head,body);$("cards").append(card);tableHost.hidden=true;
    const w={card,head,note,copy,host:chartHost,tableHost,table:false,spec,chart:null,updates:0};
    toggle.onclick=()=>{w.table=!w.table;toggle.setAttribute("aria-pressed",String(w.table));draw(w);if(!w.table)w.chart?.resize();};
    copy.onclick=()=>copyCharts(copy,[w.spec]);
    return w;
  }
  function draw(w) {
    const spec=w.spec;
    w.copy.hidden=!w.table;
    const timeRange=datasets.get(spec.series[0]?.dataset)?.range;
    // The time axis starts at the first bucket with data; a chip tells when that is later than the range.
    const first=firstData(spec.series),since=!spec.heat&&timeRange&&first>timeRange.start?first:null;
    w.note.replaceChildren(...[since?`data since ${sinceLabel(since,timeRange)}`:null,spec.note].filter(Boolean).map(n=>el("span","",n)));
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
      option={grid:{left:65,right:18,top:55,bottom:55},legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0},tooltip:{trigger:"axis",axisPointer:{type:"shadow"},valueFormatter:v=>format(v,spec.unit)},xAxis:{type:"time",min:timeRange?((since??timeRange.start)-timeRange.step/2)*1000:undefined,max:timeRange?timeRange.end*1000:undefined,axisLabel:{hideOverlap:true},axisPointer:{label:{formatter:p=>F.when(p.value)}}},yAxis:{type:"value",max:spec.max,minInterval:["count","tokens"].includes(spec.unit)?1:undefined,axisLabel:{formatter:v=>spec.unit==="ratio"?`${Math.round(v*100)}%`:spec.unit==="USD"?AIUsage.fmt.cost(v):spec.unit==="seconds"?AIUsage.fmt.duration(v):AIUsage.fmt.tokens(v)}},dataZoom:[{type:"inside"}],series:spec.series.map(s=>({id:`${s.dataset}:${s.id}`,name:s.label||s.id,type:spec.unit==="ratio"?"line":"bar",stack:spec.stack?"total":undefined,itemStyle:{color:seriesColor(s)},lineStyle:{color:seriesColor(s)},data:s.points.map(([t,v])=>[t*1000,v]),connectNulls:false}))};
    }
    Object.assign(option,{animationDurationUpdate:180,aria:{enabled:true}});
    w.chart.setOption(option,{replaceMerge:["series"],lazyUpdate:true});w.updates++;
  }
  // "Agent · model" (or kind) series get a shade of the agent hue; everything else the agent hue.
  const seriesColor=s=>AIUsage.colorFor(s.label?.includes(" · ")?s.label:agents[s.agent]||s.label||s.id);
  const firstData=series=>Math.min(...series.flatMap(s=>s.points.filter(([,v])=>v>0).map(([t])=>t)));
  const sinceLabel=(t,range)=>{const d=new Date(t*1000),two=n=>String(n).padStart(2,"0");return range.end-range.start<=86400+range.step?`${two(d.getHours())}:${two(d.getMinutes())}`:`${two(d.getDate())}.${two(d.getMonth()+1)}`;};
  function renderTable(w) {
    // Series names break only after the agent ("Claude Code ·"), after the model family ("claude-") or between label words.
    // Pieces are [text, separator]: a hyphen break adds no space, a word break keeps one.
    const name=(text,cls="tm-col-num aiu-col-name")=>{
      const th=el("th",cls);th.scope="col";
      const pieces=text.split(" · ").flatMap((part,i,all)=>{const m=part.match(/^([^-\s]+-)(\S.*)$/),tail=i<all.length-1?" ·":"";return m?[[m[1],"<wbr>"],[m[2]+tail," "]]:(part+tail).split(" ").map(w=>[w," "]);});
      th.innerHTML=pieces.map(([piece,sep],i)=>`<span>${esc(piece)}</span>${i<pieces.length-1?sep:""}`).join("");
      return th;
    };
    const table=el("table","tm-table"),thead=el("thead"),tbody=el("tbody"),head=el("tr"),bucket=name("Bucket start (local)","aiu-col-name");
    head.append(bucket,...w.spec.series.map(s=>name(s.label||s.id)));thead.append(head);table.append(thead,tbody);
    const times=[...new Set(w.spec.series.flatMap(s=>s.points.map(p=>p[0])))].sort((a,b)=>b-a),maps=w.spec.series.map(s=>new Map(s.points));
    for(const t of times){const row=el("tr");row.append(el("td","aiu-col-time",AIUsage.fmt.when(t*1000)),...maps.map(m=>el("td","tm-num",format(m.get(t),w.spec.unit))));tbody.append(row);}
    w.tableHost.replaceChildren(table);
  }
  // ---- Overview (main view). Values are backend totals; the browser only lays them out. ----
  const F=AIUsage.fmt, css=name=>getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const esc=v=>String(v).replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"})[c]);
  const two=n=>String(n).padStart(2,"0");
  const chosen=()=>Object.keys(agents).filter(a=>selection.agent==="all"||selection.agent===a);
  /** Text with a The Mechanics tooltip (hover and keyboard focus), e.g. the full value behind a short one. */
  const tip=(html,text,cls="")=>text?`<span class="tm-tip ${cls}" tabindex="0">${html}<span class="tm-tip__bubble" role="tooltip">${esc(text)}</span></span>`:html;
  /** Small identity square + label, colored by --_c. */
  const dot=(color,html)=>`<span class="aiu-dot" style="--_c:${esc(color)}">${html}</span>`;
  const swatch=color=>`<span class="tm-legend__swatch" style="--_series:${esc(color)}"></span>`;
  const legendItem=(color,html)=>`<li><span class="tm-legend__item">${swatch(color)}${html}</span></li>`;
  /** Panel head of a generated panel: title, subtitle, optional actions. */
  const panelHead=(title,sub="",actions="")=>`<header class="tm-panel__head tm-panel__head--rich"><div class="tm-panel__heading"><h3 class="tm-panel__title">${title}</h3>${sub?`<span class="tm-panel__subtitle">${sub}</span>`:""}</div>${actions?`<div class="tm-panel__actions">${actions}</div>`:""}</header>`;
  const agentColor=a=>AIUsage.colorFor(agents[a]);
  let metric="cost",trendChart,heatChart,hoursChart;
  try{metric=["tokens","types"].includes(localStorage.getItem("metric"))?localStorage.getItem("metric"):"cost";}catch(_){}
  // Token types stack to the total: output is drawn without its reasoning part.
  const TYPES=[["cached","Cached input","--aiu-tok-cached"],["input","Uncached input","--aiu-tok-uncached"],["written","Cache write","--aiu-tok-written"],["output","Output","--aiu-tok-output"],["reasoning","Reasoning","--aiu-tok-reasoning"]];
  /** Bucket times of the current range, oldest first. */
  const buckets=range=>{const out=[];for(let t=range.start;t<range.end;t+=range.step)out.push(t);return out;};
  const bucketLabel=step=>step<3600?`${step/60}-minute buckets`:step<86400?`${step/3600}-hour buckets`:step===86400?"daily buckets":step===604800?"weekly buckets":`${step/86400}-day buckets`;
  function timeLabel(t,range){
    const d=new Date(t*1000),hm=`${two(d.getHours())}:${two(d.getMinutes())}`,dm=`${two(d.getDate())}.${two(d.getMonth()+1)}`;
    return range.end-range.start<=86400+range.step?hm:range.step<86400?`${dm} ${hm}`:dm;
  }
  /** Sums point lists by time; used only to draw combined sparklines and stacks. */
  function sumPoints(lists){const m=new Map();for(const list of lists)for(const [t,v]of list||[])m.set(t,(m.get(t)||0)+v);return m;}
  /** Trend vs the previous period of the same length; good is "up", "down" or null (neutral metric). */
  function delta(current,previous,{good=null,points=false}={}){
    if(current==null||previous==null||(!points&&!previous))return `<span class="tm-trend">— n/a<span class="tm-sr-only"> (no previous period)</span></span>`;
    const change=points?(current-previous)*100:(current-previous)/previous*100;
    if(Math.abs(change)<(points?0.05:0.5))return `<span class="tm-trend">±0${points?" pp":"%"}<span class="tm-sr-only"> vs previous period</span></span>`;
    const up=change>0,text=points?`${Math.abs(change).toFixed(1)} pp`:`${Math.abs(change)>=100?Math.round(Math.abs(change)):Math.abs(change).toFixed(0)}%`;
    const tone=!good?"":(up===(good==="up"))?"tm-trend--good":"tm-trend--bad",verdict=tone==="tm-trend--good"?"improving":tone?"worsening":"change";
    return `<span class="tm-trend ${tone}">${up?"↑ +":"↓ −"}${text}<span class="tm-sr-only"> vs previous period (${verdict})</span></span>`;
  }
  /** A thin stacked bar of shares; parts are [value, color]. */
  const shareBar=parts=>{const all=parts.reduce((n,[v])=>n+(v||0),0);return all?`<div class="aiu-track aiu-track--sm" aria-hidden="true">${parts.filter(([v])=>v>0).map(([v,c])=>`<span style="width:${v/all*100}%;background:${c}"></span>`).join("")}</div>`:"";};
  /** Agent shares of one total: a stacked bar and identity-colored labels. */
  function split(key,summary){
    const parts=chosen().map(a=>({a,v:summary[a]?.[key]?.value})),total=parts.reduce((s,p)=>s+(p.v||0),0);
    if(!total)return "";
    return shareBar(parts.map(p=>[p.v,agentColor(p.a)]))+parts.map(p=>dot(agentColor(p.a),`${agents[p.a]} ${p.v==null?"not reported":Math.round(p.v/total*100)+"%"}`)).join("");
  }
  function sparkline(points,color){
    const values=points.map(([,v])=>v),max=Math.max(...values,0);
    if(values.length<2||!max)return "";
    const xy=points.map(([,v],i)=>[i*300/(points.length-1),34-v/max*30]),line=xy.map(([x,y],i)=>`${i?"L":"M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
    const [lx,ly]=xy.at(-1);
    return `<svg class="aiu-kpi__spark" viewBox="0 0 300 36" preserveAspectRatio="none" aria-hidden="true"><path d="${line} L300 36 L0 36 Z" fill="${color}" fill-opacity="${css("--tm-chart-fill-opacity")||0.2}"></path><path d="${line}" fill="none" stroke="${color}" stroke-width="1.5" vector-effect="non-scaling-stroke"></path><circle cx="${lx}" cy="${ly}" r="2.5" fill="${color}" vector-effect="non-scaling-stroke"></circle></svg>`;
  }
  /** Panel grids never leave a hole: an odd last panel of a two-column grid spans the row. */
  function balanceAll(){
    for(const grid of document.querySelectorAll('.aiu-grid[data-cols="2"]')){
      const shown=[...grid.children].filter(c=>!c.hidden);
      shown.forEach((c,i)=>c.classList.toggle("aiu-span-all",shown.length%2===1&&i===shown.length-1));
    }
    $("hours").classList.toggle("aiu-wide",$("heat-detail").hidden);
  }
  function renderOverview() {
    const summary=datasets.get("all.summary")?.details;
    if(!summary)return;
    const totals=summary.totals||{},previous=Object.fromEntries(Object.entries(summary.previous?.totals||{}).map(([k,v])=>[k,v.value])),range=datasets.get("all.summary").range;
    const sel=chosen(),tokens=sel.map(a=>datasets.get(`${a}.tokens`)).filter(d=>d&&d.state!=="unavailable");
    const category=id=>tokens.map(d=>(d.details.categories||[]).find(c=>c.id===id)?.points);
    const lead=sel.length===1?agentColor(sel[0]):css("--tm-text-2");
    // Charts and their captions sit at the bottom of every tile so all four line up.
    const kpi=(label,value,trend,chart,detail,full="",labelTip="")=>`<div class="tm-kpi"><div class="tm-kpi__label">${tip(label,labelTip)}</div><div class="tm-kpi__value">${tip(value,full)}</div><div class="tm-kpi__foot">${trend}<span class="tm-meta">vs previous period</span></div>${chart||'<div class="aiu-kpi__spark"></div>'}<div class="aiu-kpi__detail">${detail}</div></div>`;
    const spark=points=>live.map(t=>[t,points.get(t)||0]);
    const totalSeries=sumPoints(category("total")),firstActive=Math.min(...[...totalSeries].filter(([,v])=>v>0).map(([t])=>t));
    const live=buckets(range).filter(t=>t>=firstActive||!Number.isFinite(firstActive));
    // Cache hit per bucket over all selected agents, drawn on a narrow scale.
    const sums=["input","cached","written"].map(id=>sumPoints(category(id)));
    const hits=live.map(t=>{const all=sums.reduce((n,m)=>n+(m.get(t)||0),0);return [t,all>0?(sums[1].get(t)||0)/all*100:null];});
    const known=hits.filter(([,v])=>v!=null),low=known.reduce((m,h)=>h[1]<m[1]?h:m,[0,101]);
    // Near-empty buckets can drop to 0%, so the scale follows the range-wide rate and lower buckets are clamped.
    const overall=(totals.cacheHit?.value??0)*100,floor=overall>=98?98:Math.max(0,Math.floor(overall/5)*5-5);
    const hitBars=known.length?`<span class="tm-tip aiu-hits-tip" tabindex="0"><span class="aiu-hits" aria-hidden="true">${hits.map(([,v])=>v==null?`<span style="height:2px;background:var(--aiu-track)"></span>`:`<span style="height:${Math.max(2,Math.round((Math.max(v,floor)-floor)/(100-floor)*36))}px;background:${v<Math.max(floor,overall-1)?"var(--tm-warning)":"var(--aiu-tok-cached)"}"></span>`).join("")}</span><span class="tm-tip__bubble" role="tooltip">Scale ${floor}–100%; lower buckets are clamped</span></span>`:"";
    const inputs=["cached","input","written"].map(k=>totals[k]?.value||0),inputAll=inputs.reduce((s,n)=>s+n,0);
    const hitNote=inputAll?[shareBar(inputs.map((n,i)=>[n,`var(${["--aiu-tok-cached","--aiu-tok-uncached","--aiu-tok-written"][i]})`])),dot("var(--aiu-tok-cached)",`cached ${F.pct(inputs[0]/inputAll*100)}`),known.length?`<span class="aiu-push">min ${F.pct(low[1])} at ${timeLabel(low[0],range)}</span>`:""].join(""):"";
    const cost=sumPoints(sel.map(a=>datasets.get(`${a}.cost`)?.details?.timeline)),outputs=sumPoints(category("output"));
    const output=totals.output?.value,reasoning=totals.reasoning?.value,share=output&&reasoning!=null?reasoning/output:null;
    const outputBody=output?[shareBar([[reasoning||0,"var(--aiu-tok-reasoning)"],[output-(reasoning||0),"var(--aiu-tok-output)"]]),share==null?"<span>reasoning not reported</span>":dot("var(--aiu-tok-reasoning)",`reasoning ${F.tokens(reasoning)} · ${Math.round(share*100)}%`),`<span>response ${F.tokens(output-(reasoning||0))}</span>`].join(""):"";
    $("kpis").innerHTML=[
      kpi("Cost",F.cost(totals.cost?.value),delta(totals.cost?.value,previous.cost,{good:"down"}),sparkline(spark(cost),lead),split("cost",summary),F.cost(totals.cost?.value,true)),
      kpi("Tokens",F.tokens(totals.total?.value),delta(totals.total?.value,previous.total),sparkline(spark(totalSeries),lead),split("total",summary),F.full(totals.total?.value)),
      kpi("Cache hit",totals.cacheHit?.value==null?"—":F.pct(totals.cacheHit.value*100),delta(totals.cacheHit?.value,previous.cacheHit,{good:"up",points:true}),hitBars,hitNote),
      kpi("Output",F.tokens(output),delta(output,previous.output),sparkline(spark(outputs),css("--aiu-tok-output")),outputBody,F.full(output),"Output includes reasoning tokens"),
    ].join("");
    renderTrend(range,firstActive);renderShare();renderScopes(range);renderComposition(totals);renderModels();renderHours();renderHeat();renderSpikes(summary,range);renderAgents(summary,range);
    balanceAll();
  }
  let shareMetric="cost",shareChart,shareSpecs=[];
  try{shareMetric=localStorage.getItem("shareMetric")==="tokens"?"tokens":"cost";}catch(_){}
  function renderShare() {
    const d=datasets.get("all.share"),card=$("share");
    card.hidden=selection.agent!=="all"||!d;shareSpecs=[];if(card.hidden)return;
    shareSpecs=d.series.map(s=>({title:`Claude Code share of ${s.id}`,unit:"ratio",series:[{...s,label:"Claude Code share",dataset:d.id}]}));
    const range=d.range,series=d.series.find(s=>s.id===shareMetric),values=new Map(series?.points||[]);
    const first=series?.points[0]?.[0]??range.start;
    const points=buckets(range).filter(t=>t>=first).map(t=>[t*1000,values.get(t)??null]);
    const orange=agentColor("claude"),blue=agentColor("codex"),totals=d.details[shareMetric];
    const fmt=shareMetric==="cost"?v=>F.cost(v,true):F.full;
    for(const b of document.querySelectorAll("[data-share-metric]"))b.setAttribute("aria-pressed",String(b.dataset.shareMetric===shareMetric));
    $("share-sub").textContent=`Claude Code share of ${shareMetric} · ${bucketLabel(range.step)} · 50% is an even split`;
    $("share-totals").innerHTML=totals?.share!=null?["claude","codex"].map((a,i)=>legendItem(i?blue:orange,`${agents[a]} <b>${F.pct((i?1-totals.share:totals.share)*100)}</b> · ${esc(fmt(totals[a]))}`)).join("")+`<li class="tm-meta">selected range</li>`:"";
    $("share-note").textContent=["100% = Claude Code + Codex. Antigravity excluded. Empty buckets are gaps.",!series?.points.length?"Comparison unavailable: both agents need complete usage data in this scope.":"",...(shareMetric==="cost"?d.warnings:[])].filter(Boolean).join(" ");
    $("share-chart").hidden=!series?.points.length;if($("share-chart").hidden)return;
    // Fill each line segment back to the 50% baseline; split exactly at crossings.
    const areas=[];
    for(let i=1;i<points.length;i++){
      const [x,a]=points[i-1],[y,b]=points[i];if(a==null||b==null)continue;
      if((a-.5)*(b-.5)<0){const cross=x+(y-x)*(.5-a)/(b-a);areas.push([x,a,cross,.5],[cross,.5,y,b]);}
      else areas.push([x,a,y,b]);
    }
    shareChart=shareChart||AIUsage.init($("share-chart"));
    const latest=series.points.at(-1);
    shareChart.setOption({
      grid:{left:8,right:62,top:24,bottom:8,containLabel:true},
      tooltip:{trigger:"axis",formatter:params=>{const p=params.find(p=>p.seriesId==="share-line"),v=p?.value?.[1];return v==null?AIUsage.tipRow("No usage in this bucket","",{muted:true}):AIUsage.tipTitle(F.when(p.value[0]))+AIUsage.tipRow("Claude Code",F.pct(v*100),{color:orange})+AIUsage.tipRow("Codex",F.pct((1-v)*100),{color:blue});}},
      xAxis:{type:"time",min:first*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",min:0,max:1,interval:.25,axisLabel:{showMinLabel:true,showMaxLabel:true,formatter:v=>`${Math.round(v*100)}%`}},
      series:[{id:"share-fill",type:"custom",coordinateSystem:"cartesian2d",silent:true,tooltip:{show:false},data:areas,
        renderItem:(params,api)=>{const a=api.value(1),b=api.value(3),color=(a+b)/2>=.5?orange:blue,left=api.coord([api.value(0),a]),right=api.coord([api.value(2),b]);return {type:"group",children:[{type:"polygon",shape:{points:[api.coord([api.value(0),.5]),left,right,api.coord([api.value(2),.5])]},style:{fill:color,opacity:.16}},{type:"polyline",shape:{points:[left,right]},style:{stroke:color,lineWidth:2,fill:null}}]};},
      },{id:"share-line",name:"Claude Code share",type:"line",data:points,showSymbol:true,symbolSize:(value,p)=>points[p.dataIndex-1]?.[1]==null&&points[p.dataIndex+1]?.[1]==null?6:0,itemStyle:{color:p=>p.value[1]>=.5?orange:blue},connectNulls:false,lineStyle:{opacity:0},
        markLine:{silent:true,symbol:"none",label:{show:false},lineStyle:{color:css("--tm-chart-target"),type:[2,3],width:1.5},data:[{yAxis:.5}]},
        markPoint:{symbol:"circle",symbolSize:6,itemStyle:{color:latest[1]>=.5?orange:blue},label:{show:true,position:"right",color:css("--tm-text"),fontFamily:css("--tm-font-mono"),fontWeight:600,formatter:F.pct(latest[1]*100)},data:[{coord:[latest[0]*1000,latest[1]]}]},
      }],
    },{replaceMerge:["series"]});
    shareChart.resize();
  }
  function quotaOverlay(range) {
    const enabledEvents=$("limit-events").checked,enabledPeriods=$("limit-periods").checked;
    const dataset=datasets.get("all.limits"),windows=dataset?.details?.windows||[],events=[],areas=[];
    let end=range.end;
    const weekly=range.end-range.start-range.step>=7*86400;
    const now=Date.now()/1000;
    for(const w of windows){
      if(!chosen().includes(w.agent))continue;
      const history=w.history||{},current=w.current;
      const minutesAt=t=>{let v=null;for(const [at,n]of history.window_minutes||[])if(at<=t||v==null)v=n;return v??current?.window_minutes??null;};
      const relevant=m=>m>0&&(weekly?m===10080:m<1440);
      const who=`${agents[w.agent]}${w.group?" · "+w.group:""}`;
      const resets=new Map(history.reset_timestamp_seconds||[]),used=new Map(history.used_percent||[]),exhausted=new Map(history.exhausted||[]);
      if(enabledEvents){
        let wasFull=false;
        for(const t of [...new Set([...used.keys(),...exhausted.keys()])].sort((a,b)=>a-b)){
          // Agent Accounts reports exhaustion explicitly; Claude's weekly 100% stays usable until a turn is rejected.
          const full=exhausted.has(t)?exhausted.get(t)===1:(used.get(t)??0)>=100;
          // A bucket can retain pre-reset exhaustion alongside the next reset time.
          // Only observed recovery followed by exhaustion starts another episode.
          if(full&&!wasFull&&t>=range.start&&t<=range.end){
            const minutes=minutesAt(t),duration=minutes>0?(minutes%1440===0?`${minutes/1440}-day`:minutes%60===0?`${minutes/60}-hour`:`${minutes}-minute`):"";
            const kind=[duration,w.label!==duration&&w.label?.toLowerCase()!==w.agent?w.label:""].filter(Boolean).join(" · ")||w.window;
            events.push({xAxis:t*1000,name:`${who} · ${kind} · limit exhausted (observed)`,label:{formatter:`${agents[w.agent]} · ${kind}`},lineStyle:{color:AIUsage.colorFor(agents[w.agent])}});
          }
          wasFull=full;
        }
      }
      if(enabledPeriods){
        const periods=new Map();
        const addPeriod=(reset,minutes,live)=>{
          // Providers can correct the same reset by a few seconds between observations.
          for(const [key,p]of periods)if(p.minutes===minutes&&Math.abs(key-reset)<=120)periods.delete(key);
          periods.set(reset,{reset,minutes,live});
        };
        for(const [t,reset]of resets){const minutes=minutesAt(t);if(used.get(t)>0&&relevant(minutes)&&reset-t<minutes*60-120&&reset<=now&&reset>=range.start&&reset-minutes*60<=range.end)addPeriod(reset,minutes,false);}
        if(current&&current.used_percent>0&&relevant(current.window_minutes)&&current.reset_timestamp_seconds>now&&current.reset_timestamp_seconds<=now+current.window_minutes*60+3600){
          const reset=current.reset_timestamp_seconds;addPeriod(reset,current.window_minutes,true);end=Math.max(end,reset);
        }
        for(const {reset,minutes,live}of periods.values()){
          const until=live?reset:Math.min(reset,range.end),start=Math.max(range.start,reset-minutes*60);
          if(until<=start)continue;
          areas.push([{name:`${who} · ${minutes===10080?"7-day":minutes/60+"-hour"} reset ${F.when(reset*1000)}`,xAxis:start*1000,itemStyle:{color:AIUsage.colorFor(agents[w.agent]),opacity:.04}},{xAxis:until*1000}]);
        }
      }
    }
    return {events,areas,end};
  }
  // Shift+drag on the trend marks whole buckets and totals the drawn values; Esc, a plain click or ✕ clears it.
  let brush=null,trendView=null;
  const stamp=t=>F.when(t*1000);
  function brushParts(){
    const v=trendView;
    if(!brush||!v)return null;
    const lo=Math.min(brush.from,brush.to)/1000,hi=Math.max(brush.from,brush.to)/1000,half=v.range.step/2;
    const picked=v.times.filter(t=>t+half>lo&&t-half<hi);
    if(!picked.length)return null;
    const rows=v.visible.map(s=>{const m=new Map(s.points);return {name:s.name,color:s.color||AIUsage.colorFor(s.key||s.name),value:picked.reduce((n,t)=>n+(m.get(t)||0),0)};}).filter(r=>r.value>0).sort((a,b)=>b.value-a.value);
    return {start:picked[0],end:picked[picked.length-1]+v.range.step,rows,total:rows.reduce((n,r)=>n+r.value,0)};
  }
  function brushChips(parts){
    if(!parts)return "";
    const f=trendView.fmtFull;
    return `<span class="tm-chart-readout__value">${stamp(parts.start)} – ${stamp(parts.end)} · ${esc(f(parts.total))}</span>`
      +parts.rows.map(r=>dot(r.color,`${esc(r.name)} ${esc(f(r.value))}`)).join("")
      +`<button type="button" class="tm-btn tm-btn--ghost tm-btn--icon tm-btn--sm" id="brush-clear" aria-label="Clear selection (Esc)">${CLOSE_ICON}</button>`;
  }
  const brushSeries=parts=>({id:"selection",type:"line",data:[],silent:true,markArea:{silent:true,
    itemStyle:{color:css("--tm-text-2"),opacity:.12},
    data:parts?[[{xAxis:(parts.start-trendView.range.step/2)*1000},{xAxis:(parts.end-trendView.range.step/2)*1000}]]:[]}});
  function paintBrush(){
    if(!trendView)return;
    const parts=brushParts();
    $("trend-chips").innerHTML=trendView.chips+brushChips(parts);
    trendChart.setOption({series:[brushSeries(parts)]});
  }
  function clearBrush(){if(brush){brush=null;paintBrush();}}
  function bindBrush(chart){
    const zr=chart.getZr(),at=e=>chart.convertFromPixel({gridIndex:0},[e.offsetX,e.offsetY])[0];
    let dragging=false;
    zr.on("mousedown",e=>{
      if(!e.event.shiftKey||!trendView||!chart.containPixel({gridIndex:0},[e.offsetX,e.offsetY]))return;
      e.event.preventDefault();dragging=true;brush={from:at(e),to:at(e)};paintBrush();
    });
    zr.on("mousemove",e=>{if(dragging){brush.to=at(e);paintBrush();}});
    zr.on("click",e=>{if(!e.event.shiftKey)clearBrush();});
    window.addEventListener("mouseup",()=>{dragging=false;});
  }
  function renderTrend(range,firstActive) {
    const sel=chosen(),series=[];
    if(metric==="types"){
      const kind=id=>sumPoints(sel.map(a=>datasets.get(`${a}.tokens`)).filter(d=>d&&d.state!=="unavailable").map(d=>(d.details.categories||[]).find(c=>c.id===id)?.points));
      const reasoning=kind("reasoning");
      for(const [id,name,color]of TYPES)series.push({name,color:css(color),points:[...kind(id)].map(([t,v])=>[t,id==="output"?v-(reasoning.get(t)||0):v])});
    }
    else for(const a of sel){
      const d=datasets.get(`${a}.${metric==="cost"?"cost":"tokens"}`);
      if(!d||d.state==="unavailable")continue;
      const rows=metric==="cost"?d.series.map(s=>({model:s.dimensions.model,points:s.points})):(d.details.models||[]).map(s=>({model:s.id,points:s.points}));
      for(const r of rows)series.push({name:r.model||"?",key:`${agents[a]} · ${r.model||"?"}`,points:r.points});
    }
    const visible=AIUsage.nonEmpty(series.map(s=>({...s,data:s.points}))),times=buckets(range);
    const overlay=quotaOverlay(range);
    $("trend").hidden=false;
    const stack=sumPoints(visible.map(s=>s.points)),first=($("limit-events").checked||$("limit-periods").checked)?range.start:Number.isFinite(firstActive)?firstActive:range.start;
    const fmtShort=metric==="cost"?v=>F.cost(v):F.tokens,fmtFull=metric==="cost"?v=>F.cost(v,true):F.full;
    $("trend-title").textContent=metric==="cost"?"Cost over time":"Tokens over time";
    $("trend-sub").textContent=`${bucketLabel(range.step)} · stacked by ${metric==="types"?"token type":"model"} · Shift+drag to sum a span`;
    for(const b of document.querySelectorAll("[data-metric]"))b.setAttribute("aria-pressed",String(b.dataset.metric===metric));
    $("trend-legend").innerHTML=visible.map(s=>legendItem(s.color||AIUsage.colorFor(s.key||s.name),esc(s.name))).join("");
    trendView={visible,times:times.filter(t=>t>=first),range,fmtFull,chips:first>range.start?`<span>data since ${timeLabel(first,range)}</span>`:""};
    const selected=brushParts();
    $("trend-chips").innerHTML=trendView.chips+brushChips(selected);
    const peak=[...stack].filter(([t])=>t>=first).reduce((m,p)=>p[1]>m[1]?p:m,[0,0]);
    const usageTooltip=AIUsage.tooltipFormatter(fmtFull);
    const eventTooltip=events=>events.map(e=>AIUsage.tipRow(e.name,"",{color:e.lineStyle.color})+AIUsage.tipRow(F.when(e.xAxis),"",{muted:true})).join("");
    // Include every quota observation in the hovered usage bucket, even off the line itself.
    const eventsByBucket=new Map();
    for(const event of overlay.events){
      const bucket=range.start+Math.floor((event.xAxis/1000-range.start)/range.step)*range.step;
      if(!eventsByBucket.has(bucket))eventsByBucket.set(bucket,[]);
      eventsByBucket.get(bucket).push(event);
    }
    const markerEvents=[...new Set(overlay.events.map(e=>e.xAxis))].map(time=>{
      const events=overlay.events.filter(e=>e.xAxis===time);
      return {...events[0],label:{formatter:events.map(e=>e.label.formatter).join("\n")},tooltip:{trigger:"item",formatter:()=>eventTooltip(events)}};
    });
    if(!trendChart){trendChart=AIUsage.init($("trend-chart"));bindBrush(trendChart);}
    trendChart.setOption({
      grid:{left:8,right:8,top:36,bottom:8,containLabel:true},
      tooltip:{trigger:"axis",formatter:params=>usageTooltip(params)+eventTooltip(eventsByBucket.get(Number(params[0]?.axisValue)/1000)||[])},
      // Sub-daily buckets: hours as HH:MM, local midnight as a bold full date (the default shows a bare day number).
      xAxis:{type:"time",min:(first-range.step/2)*1000,max:overlay.end*1000,axisLabel:{hideOverlap:true,...(range.step<86400?{rich:{day:{fontWeight:"bold"}},formatter:v=>{const d=new Date(v);return d.getHours()||d.getMinutes()?`${two(d.getHours())}:${two(d.getMinutes())}`:`{day|${two(d.getDate())}.${two(d.getMonth()+1)}}`;}}:{})}},
      yAxis:{type:"value",axisLabel:{formatter:fmtShort}},
      // Stacks on a time axis need identical x values in every series, so all are aligned to the buckets.
      series:[...visible.map((s,i)=>AIUsage.bar(s.name,(m=>times.filter(t=>t>=first).map(t=>[t*1000,m.get(t)||0]))(new Map(s.points)),s.key||s.name,{
        id:s.name,barMaxWidth:28,...(s.color?{itemStyle:{color:s.color}}:{}),
        markPoint:i===visible.length-1&&peak[1]>0&&$("show-peak").checked?{...AIUsage.peak(()=>`peak ${timeLabel(peak[0],range)}  ${fmtShort(peak[1])}`),data:[{coord:[peak[0]*1000,peak[1]]}]}:undefined,
      })), {id:"quota-overlay",type:"line",data:[],showSymbol:false,silent:false,
        markLine:{symbol:["none","none"],lineStyle:{type:"dashed",width:1.5},label:{show:true,position:"insideEndTop",fontSize:10,color:css("--tm-text-2"),backgroundColor:css("--tm-surface"),padding:3},data:markerEvents},
        markArea:{silent:false,label:{show:false},emphasis:{itemStyle:{opacity:.12}},tooltip:{trigger:"item",formatter:p=>esc(p.name)},data:overlay.areas}
      },brushSeries(selected)],
    },{replaceMerge:["series"]});
  }
  // Breakdown by the scopes left at "all"; it follows the trend metric (token types count as tokens).
  let scopeBy="project",scopesChart;
  // Chart palette slots away from the agent hues; the largest scope gets the first color, unlabeled usage stays grey.
  const SCOPE_SLOTS=[1,4,5,8,6,7,3,2];
  try{scopeBy=localStorage.getItem("scopeBy")==="host"?"host":"project";}catch(_){}
  function renderScopes(range) {
    const d=datasets.get("all.scopes"),open=["project","host"].filter(k=>selection[k]==="*"),by=open.includes(scopeBy)?scopeBy:open[0];
    const unit=metric==="cost"?"cost":"tokens",names={host:"server",project:"project"};
    const rows=(d?.series||[]).filter(s=>s.dimensions.by===by&&s.dimensions.metric===unit).map(s=>({name:s.label,points:s.points,sum:total(s.points)})).filter(r=>r.sum>0).sort((a,b)=>b.sum-a.sum);
    $("scopes").hidden=!rows.length;if(!rows.length)return;
    $("scopes-by").hidden=open.length<2;
    for(const b of document.querySelectorAll("[data-scope-by]"))b.setAttribute("aria-pressed",String(b.dataset.scopeBy===by));
    const fmtShort=unit==="cost"?v=>F.cost(v):F.tokens,fmtFull=unit==="cost"?v=>F.cost(v,true):F.full;
    const first=Math.min(...rows.flatMap(r=>r.points.filter(([,v])=>v>0).map(([t])=>t))),times=buckets(range).filter(t=>t>=first);
    $("scopes-title").textContent=`${unit==="cost"?"Cost":"Tokens"} by ${names[by]}`;
    $("scopes-sub").textContent=`${bucketLabel(range.step)} · stacked by ${names[by]}`;
    $("scopes-chips").innerHTML=[first>range.start?`<span>data since ${timeLabel(first,range)}</span>`:"",...(unit==="cost"?d.warnings:[]).map(w=>`<span>${esc(w)}</span>`)].join("");
    scopesChart=scopesChart||AIUsage.init($("scopes-chart"));
    scopesChart.setOption({
      grid:{left:8,right:8,top:16,bottom:36,containLabel:true},
      legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0,data:rows.map(r=>r.name),formatter:name=>`${name}  ${fmtShort(rows.find(r=>r.name===name)?.sum||0)}`},
      tooltip:{trigger:"axis",formatter:AIUsage.tooltipFormatter(fmtFull)},
      xAxis:{type:"time",min:(first-range.step/2)*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",axisLabel:{formatter:fmtShort}},
      series:rows.map(r=>{const m=new Map(r.points),color=r.name==="(unattributed)"?css("--tm-text-3"):css(`--tm-chart-${SCOPE_SLOTS[rows.filter(x=>x.name!=="(unattributed)").indexOf(r)%SCOPE_SLOTS.length]}`);
        return AIUsage.bar(r.name,times.map(t=>[t*1000,m.get(t)||0]),r.name,{id:r.name,barMaxWidth:28,itemStyle:{color}});}),
    },{replaceMerge:["series"]});
  }
  function renderComposition(totals) {
    const v=k=>totals[k]?.value||0,reasoning=totals.reasoning?.value;
    const rows=[["Cached input",v("cached"),"--aiu-tok-cached"],["Uncached input",v("input"),"--aiu-tok-uncached"],["Cache write",v("written"),"--aiu-tok-written"],
      [reasoning==null?"Output":"Output (excl. reasoning)",v("output")-(reasoning||0),"--aiu-tok-output"],...(reasoning==null?[]:[["Reasoning",reasoning,"--aiu-tok-reasoning"]])];
    const all=rows.reduce((s,r)=>s+r[1],0),shown=rows.filter(r=>r[1]>0);
    const ratio=v("output")?Math.round(v("cached")/v("output")):null;
    $("composition").hidden=!all;
    const notes=[all&&!v("written")?"Cache write: 0, hidden because there is no data in this range.":"",reasoning==null&&all?"Reasoning is not reported by the selected agents.":""].filter(Boolean);
    $("composition").innerHTML=panelHead("Token composition",ratio!=null?`each output token comes with ${F.full(ratio)} cached input tokens`:"")+`<div class="tm-panel__body">
      ${all?`<div class="aiu-rows">${shown.map(([label,n,color])=>{const p=n/all*100;return `<div class="aiu-row"><div class="aiu-row__head"><span class="aiu-row__name"><span class="aiu-swatch" style="--_c:var(${color})"></span>${label}</span><span class="aiu-row__value">${F.full(n)}<small>${p<1?p.toFixed(2):p.toFixed(1)}%</small></span></div><div class="aiu-track" aria-hidden="true"><span style="width:${Math.max(0.8,p)}%;background:var(${color})"></span></div></div>`;}).join("")}</div>`:`<p class="tm-meta">No token usage in this range.</p>`}
      ${notes.map(n=>`<p class="tm-meta aiu-note">${n}</p>`).join("")}</div>`;
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
    $("models").innerHTML=panelHead("Cost by model",`${active.length} of ${rows.length} models active in this range`,`<span class="tm-panel__meta">USD</span>`)+`<div class="tm-panel__body"><div class="aiu-rows">${active.map(r=>{const color=AIUsage.colorFor(`${agents[r.agent]} · ${r.model}`);return `<div class="aiu-row"><div class="aiu-row__head"><span class="aiu-row__name"><span class="aiu-swatch" style="--_c:${color}"></span>${esc(r.model)}</span><span class="aiu-row__value">${r.cost==null?"—":tip(F.cost(r.cost),F.cost(r.cost,true))}</span></div>
        <div class="aiu-track" aria-hidden="true"><span style="width:${max&&r.cost?Math.max(1,r.cost/max*100):0}%;background:${color}"></span></div>
        <div class="aiu-row__meta"><span>${tip(`${F.tokens(r.tokens)} tokens`,`${F.full(r.tokens)} tokens`)}</span>${r.cost&&r.tokens?`<span>${F.cost(r.cost/r.tokens*1e6)} / 1M</span>`:""}${["codex","antigravity"].includes(r.agent)?`<span>${r.cost==null?"no price":"estimated price"}</span>`:""}</div></div>`;}).join("")}
      ${idle.map(r=>`<div class="aiu-row aiu-row--idle"><div class="aiu-row__head"><span class="aiu-row__name"><span class="aiu-swatch" style="--_c:var(--tm-text-3)"></span>${esc(r.model)}</span><span class="aiu-row__value">—</span></div></div>`).join("")}</div></div>`;
  }
  function renderHours() {
    const d=datasets.get("all.activeHours"),available=d&&d.state!=="unavailable"&&d.series.length>0;
    const hours=v=>`${F.full(Math.round(v*100)/100)} h`;
    $("hours-chart").hidden=!available;
    $("hours-empty").hidden=!!available;
    $("hours-empty").textContent=d?.warnings?.join(" · ")||"No activity observations are available for this selection.";
    $("hours-total").innerHTML=tip(d?.total==null?"—":`≈ ${hours(d.total)}`,"Overall estimated time; overlapping agents count once","aiu-tip-end");
    const rows=chosen().map(agent=>({agent,series:d?.series.find(s=>s.id===agent)}));
    $("hours-legend").innerHTML=rows.map(({agent,series})=>legendItem(agentColor(agent),`${agents[agent]} <span class="tm-mono">${series?`≈ ${hours(total(series.points))}`:"not reported"}</span>`)).join("");
    if(!available){$("hours-sub").textContent="estimated active hours · selected range";return;}
    const range=d.range;
    $("hours-sub").textContent=`estimated active hours · ${bucketLabel(range.step)} · through ${F.when(range.end*1000)}`;
    hoursChart=hoursChart||AIUsage.init($("hours-chart"));
    hoursChart.setOption({
      grid:{left:8,right:8,top:16,bottom:12,containLabel:true},
      tooltip:{trigger:"axis",formatter:AIUsage.tooltipFormatter(hours)},
      xAxis:{type:"time",min:range.start*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",min:0,axisLabel:{formatter:hours}},
      series:rows.filter(r=>r.series).map(({agent,series},i)=>{const color=agentColor(agent);return {
        id:agent,name:agents[agent],type:"line",showSymbol:false,smooth:false,connectNulls:false,
        itemStyle:{color},lineStyle:{color,width:2,type:[ "solid",[6,3],[2,3]][i%3]},areaStyle:{color,opacity:.08},
        data:series.points.map(([t,v])=>[t*1000,v]),
      };}),
    },{replaceMerge:["series"]});
    hoursChart.resize();
  }
  function renderHeat() {
    const activity=datasets.get("all.activity");if(!activity)return;
    // Cells sum every agent; the tooltip splits each cell back per agent.
    const cells=new Map(),split=new Map();for(const s of activity.series)for(const [t,v]of s.points){const d=new Date(t*1000),key=`${(d.getDay()+6)%7},${d.getHours()}`,parts=split.get(key)||{};cells.set(key,(cells.get(key)||0)+v);parts[s.dimensions.model]=(parts[s.dimensions.model]||0)+v;split.set(key,parts);}
    const data=[];for(let day=0;day<7;day++)for(let hour=0;hour<24;hour++)data.push([hour,day,cells.get(`${day},${hour}`)||0]);
    const days=["Mon","Tue","Wed","Thu","Fri","Sat","Sun"];
    $("heat-detail").hidden=!data.some(v=>v[2]>0);if($("heat-detail").hidden)return;
    // The heatmap reads a separate hourly block capped at 30 days (7 at least), so its window is stated.
    $("heat-sub").textContent=`tokens by weekday and hour · local time · last ${Math.round((activity.range.end-activity.range.start)/86400)} days`;
    heatChart=heatChart||AIUsage.init($("heat-chart"));
    heatChart.setOption({grid:{left:4,right:4,top:4,bottom:4,containLabel:true},tooltip:{trigger:"item",formatter:p=>{const parts=Object.entries(split.get(`${p.value[1]},${p.value[0]}`)||{}).filter(([,v])=>v>0);return AIUsage.tipTitle(`${days[p.value[1]]} ${two(p.value[0])}:00`)+AIUsage.tipRow("Tokens",F.full(p.value[2]))+parts.map(([a,v])=>AIUsage.tipRow(agents[a],F.full(v),{color:agentColor(a)})).join("");}},
      xAxis:{type:"category",data:Array.from({length:24},(_,h)=>two(h)),axisLine:{show:false},axisLabel:{interval:2}},yAxis:{type:"category",inverse:true,data:days,axisLine:{show:false},axisLabel:{color:css("--tm-text")}},
      visualMap:{show:false,min:0,max:Math.max(1,...data.map(v=>v[2]))},series:[{id:"heat",type:"heatmap",data}]});
  }
  function renderSpikes(summary,range) {
    const items=(datasets.get("all.spikes")?.details||[]).slice().sort((a,b)=>b.factor-a.factor).slice(0,4),weekday=new Intl.DateTimeFormat("en",{weekday:"short"});
    const now=chosen().map(a=>{const tokens=summary[a]?.total?.value,sessions=summary[a]?.sessions;
      return `<div>${dot(agentColor(a),agents[a])}<span>${!tokens?"no activity":sessions?.value==null?"sessions not reported":`${F.full(sessions.value)} new sessions`}</span></div>`;}).join("");
    $("spikes").innerHTML=panelHead("Usage spikes","days above 2.5× the preceding 7-day average")+`<div class="tm-panel__body">
      ${items.length?`<div class="aiu-spikes">${items.map(v=>{const d=new Date(v.time*1000);return `<div class="aiu-spike"><span class="aiu-spike__factor">×${v.factor.toFixed(1)}</span><span>${weekday.format(d)}, ${two(d.getDate())}.${two(d.getMonth()+1)}</span><span class="aiu-spike__tokens">${tip(`${F.tokens(v.tokens)} tokens`,`${F.full(v.tokens)} tokens`,"aiu-tip-end")}</span></div>`;}).join("")}</div>`:`<p class="tm-meta">No spikes in the daily data for this range.</p>`}
      <div class="aiu-now"><div class="tm-kpi__label">Agents in this range</div>${now}</div></div>`;
  }
  // ---- Agent details: shared comparison, then one panel per agent built from what it reports. ----
  let agentTab="codex";
  try{agentTab=localStorage.getItem("agentTab")||agentTab;}catch(_){}
  // Codex reports per-turn histograms; Claude Code per-request averages from its api_request events.
  const LATENCY=[["firstToken","To first token"],["firstOutput","To first output item"],["turnDuration","Turn duration"],["toolDuration","Tool duration"],["requestFirstToken","Request to first token","average"],["requestDuration","API request duration","average"]];
  const COUNTS=[["active","Active time","split: user / cli"],["commits","Commits","count in range"],["lines","Lines of code","added / removed"],["sessions","New sessions","started in range"],["turns","Turns","conversation turns"],["invocations","Model invocations","completed in range"],["tools","Tool calls","calls · failed"]];
  const total=points=>(points||[]).reduce((s,[,v])=>s+(v||0),0);
  function miniBars(points,range,color) {
    const first=Math.min(...points.filter(([,v])=>v>0).map(([t])=>t));
    if(!Number.isFinite(first))return "";
    const values=new Map(points),times=buckets(range).filter(t=>t>=first),max=Math.max(...times.map(t=>values.get(t)||0));
    return `<div class="aiu-mini" aria-hidden="true">${times.map((t,i)=>`<span style="height:${Math.max(values.get(t)?6:2,Math.round((values.get(t)||0)/max*100))}%;background:${values.get(t)?color:"var(--aiu-track)"};opacity:${i===times.length-1?1:.6}"></span>`).join("")}</div>
      <div class="aiu-mini-axis" aria-hidden="true"><span>${timeLabel(times[0],range)}</span><span>${times.length>1?timeLabel(times[times.length-1],range):""}</span></div>`;
  }
  const metricCard=(label,value,note,body,full="")=>`<div class="tm-kpi"><div class="tm-kpi__label">${label}</div><div class="tm-kpi__value">${tip(value,full)}</div><div class="aiu-metric__note">${note||"&nbsp;"}</div>${body}</div>`;
  function workCard(d,range,color) {
    // Turn time is grouped by local day, or by week/month when the range has too many days.
    const groups=new Map(),days=(range.end-range.start)/86400,unit=days<=14?"day":days<=120?"week":"month";
    for(const [t,v]of d.details.totalTimeline||[]){
      const x=new Date(t*1000);if(unit==="week")x.setDate(x.getDate()-(x.getDay()+6)%7);
      const key=unit==="month"?`${x.getFullYear()}-${two(x.getMonth()+1)}`:`${x.getFullYear()}-${two(x.getMonth()+1)}-${two(x.getDate())}`;
      groups.set(key,(groups.get(key)||0)+v);
    }
    const rows=[...groups].filter(([,v])=>v>0).sort(),max=Math.max(...rows.map(([,v])=>v),0),weekday=new Intl.DateTimeFormat("en",{weekday:"short"});
    const label=key=>{const [y,m,day]=key.split("-");return unit==="month"?`${m}.${y}`:`${unit==="week"?"wk ":weekday.format(new Date(+y,m-1,+day))+" "}${day}.${m}`;};
    return `<article class="tm-panel">${panelHead("Agent working time",`Σ turn duration, including tools and waiting · by ${unit}`,`<span class="aiu-figure">${tip(F.duration(d.details.total_seconds),`${F.full(Math.round(d.details.total_seconds))} s`,"aiu-tip-end")}</span>`)}
      <div class="tm-panel__body"><div class="aiu-bars">${rows.map(([key,v])=>`<div class="aiu-bar"><span>${label(key)}</span><div class="aiu-track" aria-hidden="true"><span style="width:${Math.max(1,v/max*100)}%;background:${color}"></span></div><span>${F.duration(v)}</span></div>`).join("")||`<p class="tm-meta">No completed turns in this range.</p>`}</div></div></article>`;
  }
  function toolsCard(d,color) {
    const ranking=d.details.ranking||[],calls=ranking.reduce((s,r)=>s+r.calls,0),failed=ranking.reduce((s,r)=>s+r.failed,0),max=ranking[0]?.calls||0;
    const row=r=>`<div class="aiu-bar"><span>${tip(esc(r.tool),r.tool)}</span><div class="aiu-track" aria-hidden="true"><span style="width:${(r.calls-r.failed)/max*100}%;background:${color}"></span><span style="width:${r.failed?Math.max(1.2,r.failed/max*100):0}%;background:var(--tm-warning)"></span></div><span>${tip(F.full(r.calls),`${F.full(r.failed)} failed`,"aiu-tip-end")}</span></div>`;
    return `<article class="tm-panel">${panelHead("Tool calls",`${F.full(calls)} calls · ranking by tool`,calls&&failed?`<span class="tm-badge tm-badge--warning">${F.full(failed)} failed · ${F.pct(failed/calls*100,2)}</span>`:"")}
      <div class="tm-panel__body"><div class="aiu-bars">${ranking.slice(0,10).map(row).join("")}</div>
      ${ranking.length>10?`<details class="aiu-more-tools"><summary class="tm-meta">${ranking.length-10} more ${ranking.length===11?"tool":"tools"}</summary><div class="aiu-bars">${ranking.slice(10).map(row).join("")}</div></details>`:""}${failed?`<p class="tm-meta aiu-note">${dot("var(--tm-warning)","the warning-colored part of a bar is that tool's failed calls")}</p>`:""}</div></article>`;
  }
  /** Tiles for every measurement this agent reports; returns placeholders for the empty state too. */
  function agentPanel(agent,summary,range) {
    const color=agentColor(agent),small=[],wide=[],placeholders=[];let activity=["total","cost"].some(key=>summary[agent]?.[key]?.value>0);
    // Histogram quantiles when the agent exports buckets; the Σsum/Σcount mean otherwise.
    const at=(d,q)=>`${d.details[q+"Capped"]?"≥ ":""}${F.duration(d.details[q])}`;
    for(const [m,label,hint]of LATENCY){const d=reported(agent,m);if(!d)continue;placeholders.push([label,hint||"median and p95"]);
      if(d.details.p50!=null){activity=true;small.push(metricCard(label,at(d,"p50"),`p50 · p95 ${at(d,"p95")} · mean ${F.duration(d.total)}`,miniBars(d.details.p50Timeline||[],range,color),`${F.full(Math.round(d.details.p50*1000))} ms`));}
      else if(d.total!=null){activity=true;small.push(metricCard(label,F.duration(d.total),"average",miniBars(d.series[0]?.points||[],range,color),`${F.full(Math.round(d.total*1000))} ms`));}}
    for(const [m,label,hint]of COUNTS){const d=reported(agent,m);if(!d)continue;placeholders.push([label,hint]);
      const sum=total(d.details?.timeline);if(!sum)continue;activity=true;
      const kinds=d.series.map(s=>[s.dimensions.kind,total(s.points)]).filter(([,v])=>v>0);
      const value=m==="active"?F.duration(sum):m==="lines"?`+${F.tokens(summary[agent]?.added?.value||0)} / −${F.tokens(summary[agent]?.removed?.value||0)}`:F.full(sum);
      const failed=m==="tools"?total(d.details.failures):0;
      const note=m==="active"&&kinds.length?kinds.map(([k,v])=>`${esc(k)} ${F.duration(v)}`).join(" · "):m==="lines"?"added / removed":m==="tools"?`${F.full(failed)} failed`:"";
      small.push(metricCard(label,value,note,miniBars(d.details.timeline,range,color),m==="active"?`${F.full(Math.round(sum))} s`:F.full(sum)));}
    const turn=reported(agent,"turnDuration");if(turn&&turn.details.total_seconds){activity=true;wide.push(workCard(turn,range,color));}
    const tools=reported(agent,"tools");if(tools?.details.ranking?.length){activity=true;wide.push(toolsCard(tools,color));}
    return {activity,count:placeholders.length,html:small.length||wide.length?`${small.length?`<div class="tm-cells aiu-metrics">${small.join("")}</div>`:""}${wide.length?`<div class="tm-panel-grid aiu-grid" data-cols="2">${wide.join("")}</div>`:""}`:"",placeholders};
  }
  function renderAgents(summary,range) {
    const sel=chosen(),both=sel.length>1;
    if(!sel.includes(agentTab))agentTab=sel[0];
    const panels=Object.fromEntries(sel.map(a=>[a,agentPanel(a,summary,range)])),agent=both?agentTab:sel[0],panel=panels[agent];
    const rows=[["Cost","cost",v=>F.cost(v),v=>F.cost(v,true)],["Tokens","total",F.tokens,F.full],["Cache hit","cacheHit",v=>F.pct(v*100),v=>F.pct(v*100,3)],["Output","output",F.tokens,F.full],["Reasoning","reasoning",F.tokens,F.full],["New sessions","sessions",F.full,F.full]];
    $("compare-region").hidden=!both;
    if(both){
      $("compare-legend").innerHTML=sel.map(a=>legendItem(agentColor(a),agents[a])).join("");
      $("compare").innerHTML=rows.map(([label,key,short,full])=>{const values=sel.map(a=>summary[a]?.[key]),max=Math.max(...values.map(v=>v?.value||0));
        return `<div class="aiu-compare__row" style="--agents:${sel.length}"><span>${label}</span>${sel.map((a,i)=>{const v=values[i],missing=v?.state==="unavailable"||v==null;const text=v?.value!=null?short(v.value):missing?"not reported":key==="cacheHit"?"—":short(0);
          const width=v?.value?key==="cacheHit"?v.value*100:v.value/max*100:0;
          return `<div class="aiu-compare__cell"><div class="aiu-track aiu-track--sm" aria-hidden="true"><span style="width:${width}%;background:${agentColor(a)}"></span></div><span class="${v?.value==null?"is-missing":""}">${v?.value!=null?tip(text,full(v.value),"aiu-tip-end"):text}</span></div>`;}).join("")}</div>`;}).join("");
    }
    const lists=[...datasets.values()].filter(d=>d.agent===agent&&d.state!=="unavailable").map(d=>d.details?.timeline||d.details?.categories?.find(c=>c.id==="total")?.points||[]);
    const first=Math.min(...lists.flat().filter(([,v])=>v>0).map(([t])=>t)),days=Math.ceil((range.end-first)/86400);
    $("agent-meta").textContent=!panel.activity?"no data in this range":Number.isFinite(first)&&first>range.start?`data since ${timeLabel(first,range)}${range.end-range.start>86400+range.step?` · axis trimmed to ${days} ${days===1?"day":"days"}`:""}`:"";
    $("agent-title").innerHTML=both?`<span class="tm-tabs" role="tablist" aria-label="Agent">${sel.map(a=>`<button type="button" class="tm-tab" role="tab" data-tab="${a}" aria-selected="${a===agent}" tabindex="${a===agent?0:-1}"><span class="aiu-tab-mark" style="--_c:${agentColor(a)}"></span>${agents[a]} <span class="aiu-tab-count">${panels[a].count} ${panels[a].count===1?"metric":"metrics"}</span></button>`).join("")}</span>`
      :`<span class="aiu-tab-mark" style="--_c:${agentColor(agent)}"></span>${agents[agent]} · agent metrics`;
    const longer=["90d","180d","1y","all"].includes(selection.range)||CUSTOM.test(selection.range);
    const emptyTitle=panel.activity?`No ${agents[agent]} agent-specific metrics in this range`:`No ${agents[agent]} activity in this range`;
    const emptyNote=panel.activity?"Token usage or cost is available above. Timing, session and tool metrics are reported separately; none are available for the selected range and filters.":panel.placeholders.length?"When agent-specific metrics are reported for the selected range and filters, they appear here:":`${agents[agent]} reports no agent-specific metrics.`;
    const body=panel.html||`<div class="tm-state aiu-state"><div class="tm-state__title">${emptyTitle}</div><div class="tm-state__body">${emptyNote}</div>
      ${panel.placeholders.length?`<div class="aiu-placeholders">${panel.placeholders.map(([label,hint])=>`<div><span class="tm-kpi__label">${label}</span><span class="tm-mono">—</span><span>${hint}</span></div>`).join("")}</div>`:""}${longer?"":`<button type="button" class="tm-btn tm-btn--outline tm-btn--sm" data-goto="90d">Show the last 90 days</button>`}</div>`;
    const latency=LATENCY.map(([m])=>reported(agent,m)).filter(d=>d?.total!=null),quantiles=latency.some(d=>d.details.p50!=null);
    $("agents").setAttribute("role",both?"tabpanel":"group");
    $("agents").innerHTML=`${body}${latency.length&&panel.activity?`<p class="tm-meta aiu-agent-foot">${quantiles?"Durations are p50 with p95 beside it, approximated from the histogram buckets; ≥ marks a value in the open top bucket. Bars show p50 per bucket.":"Durations are averages (Σ sum / Σ count) of the duration histograms."}</p>`:""}`;
  }
  // ---- Per-model views. Categories and hit rates are Rust projections; the browser only picks and lays out. ----
  let modelKey="";
  try{modelKey=localStorage.getItem("model")||"";}catch(_){}
  const modelViews={tokens:{id:"model-tokens",table:false,spec:null,chart:null},hit:{id:"model-hit",table:false,spec:null,chart:null},effort:{id:"model-effort",table:false,spec:null,chart:null}};
  const PARTS=TYPES.filter(([id])=>id!=="reasoning");
  const pointsOf=(d,model,category)=>d.series.find(s=>s.dimensions.model===model&&s.dimensions.category===category)?.points||[];
  const viewTip=(t,rows)=>AIUsage.tipTitle(F.when(t*1000))+rows.join("");
  const tipRow=(left,right,muted,color)=>AIUsage.tipRow(left,right,{muted,color,html:true});
  /** Bucket runs without any reported sample, drawn as shaded "no data" areas. */
  function missingAreas(times,seen,range){
    const runs=[];let run=null;
    for(const t of times){if(!seen(t)){run=run||[t,t];run[1]=t;}else if(run){runs.push(run);run=null;}}
    if(run)runs.push(run);
    return runs.length?{silent:true,label:{position:"insideTop"},data:runs.map(([a,b])=>[{xAxis:(a-range.step/2)*1000,name:(b-a)/range.step>=2?"no data":""},{xAxis:(b+range.step/2)*1000}])}:undefined;
  }
  function viewFrame(v,visible){
    const card=$(v.id);card.hidden=!visible;
    if(!visible){v.spec=null;return false;}
    card.querySelector(".copy").hidden=!v.table;$(v.id+"-chart").hidden=v.table;v.tableHost=card.querySelector(".aiu-table");v.tableHost.hidden=!v.table;
    if(v.table){renderTable(v);return false;}
    v.chart=v.chart||AIUsage.init($(v.id+"-chart"));
    return true;
  }
  function renderModelViews(){renderModelTokens();renderModelHit();renderEffort();balanceAll();}
  function renderModelTokens(){
    const v=modelViews.tokens,list=[];
    for(const a of chosen()){const d=reported(a,"tokens");if(d)for(const m of d.details.models||[]){const n=total(m.points);if(n>0)list.push({key:`${a}|${m.id}`,agent:a,model:m.id,tokens:n,d});}}
    // Options stay alphabetical; without a choice the busiest model is shown.
    list.sort((x,y)=>(x.model||"?").localeCompare(y.model||"?")||x.agent.localeCompare(y.agent));
    const current=list.find(m=>m.key===modelKey)||list.reduce((a,m)=>m.tokens>a.tokens?m:a,list[0]),pick=$("model-pick");
    if(list.length&&JSON.stringify([...pick.options].map(o=>o.value))!==JSON.stringify(list.map(m=>m.key)))
      pick.replaceChildren(...list.map(m=>{const o=el("option","",m.model||"?");o.value=m.key;return o;}));
    if(current)pick.value=current.key;
    // The table/export spec is built before the frame so a table view and Export all stay current.
    if(current){
      const {d,agent,model}=current,codex=agent==="codex";
      v.spec={title:`Model tokens · ${agents[agent]} · ${model||"?"}`,unit:"tokens",export:{agent,model},
        series:[...PARTS.map(([id,name])=>({id,label:name,points:pointsOf(d,model,id),dataset:d.id})),...(codex?[{id:"reasoning",label:"Reasoning (subset of output)",points:pointsOf(d,model,"reasoning"),dataset:d.id}]:[]),{id:"total",label:"Total",points:pointsOf(d,model,"total"),dataset:d.id}]};
    }
    if(!viewFrame(v,!!current))return;
    const {d,agent,model}=current,range=d.range,maps=Object.fromEntries(v.spec.series.map(s=>[s.id,new Map(s.points)]));
    // A bucket without any reported sample is missing; a reported bucket can still be zero.
    const seen=t=>maps.total.has(t),first=Math.min(...[...maps.total].filter(([,n])=>n>0).map(([t])=>t));
    const times=buckets(range).filter(t=>t>=first),sum=total(v.spec.series.at(-1).points);
    const hit=(d.details.modelCacheHit||[]).find(m=>m.id===model);
    $("model-tokens-sub").textContent=`${bucketLabel(range.step)} · ${F.full(sum)} tokens${hit?` · cache hit ${F.pct(hit.hit*100)}`:""}`;
    $("model-tokens-chips").innerHTML=first>range.start?`<span>data since ${timeLabel(first,range)}</span>`:"";
    const areas=missingAreas(times,seen,range);
    v.chart.setOption({
      grid:{left:8,right:8,top:16,bottom:36,containLabel:true},
      legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0},
      tooltip:{trigger:"axis",formatter:params=>{
        const t=params[0].value[0]/1000;
        if(!seen(t))return viewTip(t,[tipRow("no data reported","",true)]);
        const all=PARTS.reduce((n,[id])=>n+(maps[id].get(t)||0),0);
        const rows=params.map(p=>{const id=PARTS.find(([,name])=>name===p.seriesName)?.[0],n=maps[id]?.get(t)||0;
          return tipRow(esc(p.seriesName),`${F.full(n)}${all?` · ${F.pct(n/all*100)}`:""}`,false,p.color)+(id==="output"&&agent==="codex"?tipRow("of which reasoning",F.full(maps.reasoning.get(t)||0),true):"");}).reverse();
        return viewTip(t,[...rows,tipRow("<b>Total</b>",`<b>${F.full(maps.total.get(t)||0)}</b>${all?"":" · reported, no usage"}`)]);
      }},
      xAxis:{type:"time",min:(times[0]-range.step/2)*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",minInterval:1,axisLabel:{formatter:F.tokens}},
      // Mutually exclusive categories stack to the model's tokens; reasoning stays inside output.
      series:PARTS.map(([id,name,color],i)=>AIUsage.bar(name,times.map(t=>[t*1000,seen(t)?maps[id].get(t)||0:null]),name,{id,barMaxWidth:28,itemStyle:{color:css(color)},markArea:i===0?areas:undefined})),
    },{replaceMerge:["series"]});
  }
  function renderModelHit(){
    const v=modelViews.hit,rows=[];
    for(const a of chosen()){const d=reported(a,"tokens");if(d)for(const m of d.details.modelCacheHit||[])rows.push({...m,name:m.label||"?",colorKey:`${agents[a]} · ${m.label||"?"}`,d});}
    rows.sort((x,y)=>(y.input+y.cached+y.written)-(x.input+x.cached+x.written));
    v.spec=null;
    if(rows.length)v.spec={title:"Cache hit by model",unit:"ratio",export:{formula:"cached_input / (uncached_input + cached_input + cache_write)"},
      series:rows.map(r=>({id:r.name,label:r.name,colorKey:r.colorKey,points:r.points,dataset:r.d.id,export:{overall:r.hit,uncached_input:r.input,cached_input:r.cached,cache_write:r.written}}))};
    const spec=v.spec;
    if(!viewFrame(v,rows.length>0))return;
    const range=rows[0].d.range,first=Math.min(...rows.flatMap(r=>r.points.map(([t])=>t))),times=buckets(range).filter(t=>t>=first);
    // Keyed by name: replaceMerge can leave holes in series indices after a model disappears.
    const counts=new Map(rows.map(r=>[r.name,Object.fromEntries(["input","cached","written","total"].map(c=>[c,new Map(pointsOf(r.d,r.id,c))]))]));
    const overall=new Map(rows.map(r=>[r.name,r.hit]));
    $("model-hit-sub").textContent=`${bucketLabel(range.step)} · cached ÷ (uncached + cached + cache write) · gaps have no input`;
    $("model-hit-chips").innerHTML=first>range.start?`<span>data since ${timeLabel(first,range)}</span>`:"";
    v.chart.setOption({
      grid:{left:8,right:8,top:16,bottom:36,containLabel:true},
      legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0,formatter:name=>`${name}  ${F.pct(overall.get(name)*100)}`},
      tooltip:{trigger:"axis",axisPointer:{type:"line"},formatter:params=>{
        const t=params[0].value[0]/1000;
        // Buckets with samples but no input say so; models without samples in the bucket are left out.
        const lines=params.map(p=>{const c=counts.get(p.seriesName);
          if(p.value[1]==null)return c.total.has(t)?tipRow(esc(p.seriesName),"no input tokens",true,p.color):"";
          return tipRow(esc(p.seriesName),`<b>${F.pct(p.value[1],2)}</b>`,false,p.color)+tipRow("cached · uncached · write",`${F.full(c.cached.get(t)||0)} · ${F.full(c.input.get(t)||0)} · ${F.full(c.written.get(t)||0)}`,true);
        }).filter(Boolean);
        return viewTip(t,lines.length?lines:[tipRow("no data reported","",true)]);
      }},
      xAxis:{type:"time",min:(times[0]-range.step/2)*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",min:0,max:100,interval:25,axisLabel:{formatter:v=>`${v}%`}},
      series:spec.series.map(s=>{const m=new Map(s.points),color=AIUsage.colorFor(s.colorKey);
        return {id:s.id,name:s.label,type:"line",smooth:false,symbol:"circle",symbolSize:4,connectNulls:false,itemStyle:{color},lineStyle:{color},data:times.map(t=>[t*1000,m.has(t)?m.get(t)*100:null])};}),
    },{replaceMerge:["series"]});
  }
  /** Copies one chart as JSON, or downloads all charts with shared provenance stored once. */
  // Effort levels from light to dark within the model hue; an unreported effort is grey.
  const EFFORTS=["minimal","low","medium","high","xhigh","max"],EFFORT_SHADE=[.7,.5,.2,-.15,-.4,-.6];
  function shade(hex,f){
    const n=parseInt(hex.replace("#",""),16),mix=c=>Math.round(f>0?c+(255-c)*f:c*(1+f));
    return "#"+[n>>16&255,n>>8&255,n&255].map(c=>mix(c).toString(16).padStart(2,"0")).join("");
  }
  const effortColor=(agent,model,effort)=>{const i=EFFORTS.indexOf(effort);return i<0?css("--tm-text-3"):shade(AIUsage.colorFor(`${agents[agent]} · ${model}`),EFFORT_SHADE[i]);};
  function renderEffort(){
    const v=modelViews.effort,sel=chosen(),rows=[];
    for(const a of sel){
      const d=reported(a,"effort");if(!d)continue;
      for(const s of d.series)if(total(s.points)>0)rows.push({agent:a,model:s.dimensions.model,effort:s.dimensions.effort,d,id:`${a}|${s.id}`,points:s.points});
    }
    const rank=e=>{const i=EFFORTS.indexOf(e);return i<0?EFFORTS.length:i;};
    rows.sort((x,y)=>x.agent.localeCompare(y.agent)||(x.model||"").localeCompare(y.model||"")||rank(x.effort)-rank(y.effort));
    const name=r=>`${r.model||"model not reported"} · ${r.effort||"not reported"}`;
    v.spec=rows.length?{title:"Tokens by effort",unit:"tokens",series:rows.map(r=>({id:r.id,label:name(r),points:r.points,dataset:r.d.id}))}:null;
    if(!viewFrame(v,rows.length>0))return;
    const range=rows[0].d.range,first=Math.min(...rows.flatMap(r=>r.points.filter(([,n])=>n>0).map(([t])=>t))),times=buckets(range).filter(t=>t>=first);
    const all=rows.reduce((n,r)=>n+total(r.points),0);
    $("model-effort-sub").textContent=`${bucketLabel(range.step)} · ${F.full(all)} tokens · reasoning effort of each response`;
    $("model-effort-chips").innerHTML=first>range.start?`<span>data since ${timeLabel(first,range)}</span>`:"";
    v.chart.setOption({
      grid:{left:8,right:8,top:16,bottom:36,containLabel:true},
      legend:{type:"scroll",left:"center",right:"auto",top:"auto",bottom:0},
      tooltip:{trigger:"axis",formatter:AIUsage.tooltipFormatter(F.full)},
      xAxis:{type:"time",min:(times[0]-range.step/2)*1000,max:range.end*1000,axisLabel:{hideOverlap:true}},
      yAxis:{type:"value",minInterval:1,axisLabel:{formatter:F.tokens}},
      series:rows.map(r=>{const m=new Map(r.points);return AIUsage.bar(name(r),times.map(t=>[t*1000,m.get(t)||0]),name(r),{id:r.id,barMaxWidth:28,itemStyle:{color:effortColor(r.agent,r.model,r.effort)}});}),
    },{replaceMerge:["series"]});
  }
  async function copyCharts(button,specs,download=false) {
    const id=snapshot,chosen=structuredClone(specs),scope={...selection};
    if(!id){toast({tone:"info",title:download?"Export":"Copy",body:"Waiting for the first data snapshot."});return;}
    button.setAttribute("aria-busy","true");
    try {
      const response=await fetch(`/api/dashboard/export?${new URLSearchParams({snapshot:id})}`),source=await response.json();if(!response.ok)throw Error(source.error||"Export failed");
      const shared={snapshot:id,selection:scope,freshness:source.freshness,queries:source.queries,warnings:source.warnings};
      const charts=chosen.map(chart=>({chart:chart.title,unit:chart.unit,time_boundaries:source.datasets.find(d=>d.id===chart.series[0]?.dataset)?.range,...chart.export,series:chart.series.map(s=>({name:s.label||s.id,values:s.points,...s.export}))}));
      if(download){
        const d=new Date(),stamp=`${d.getFullYear()}${two(d.getMonth()+1)}${two(d.getDate())}-${two(d.getHours())}${two(d.getMinutes())}${two(d.getSeconds())}`;
        const link=el("a");link.href=URL.createObjectURL(new Blob([JSON.stringify({format:"ai-usage",version:1,...shared,charts})+"\n"],{type:"application/json"}));link.download=`ai-usage-${stamp}.json`;
        link.click();setTimeout(()=>URL.revokeObjectURL(link.href),0);toast({tone:"success",title:"Exported",body:`${charts.length} charts saved to ${link.download}.`});
      }
      else{await navigator.clipboard.writeText(JSON.stringify({...shared,...charts[0]},null,2));toast({tone:"success",title:"Copied",body:`${charts[0].chart} data copied as JSON.`});}
    }catch(error){toast({tone:"danger",title:download?"Export failed":"Copy failed",body:error.message,timeout:0});}
    finally{button.removeAttribute("aria-busy");}
  }
  window.usage={initial,select,apply,custom,rangeLabel,restore(){theme();},debug(){return {epoch,snapshot,sourceAt,datasets,widgets,modelViews};}};
  document.addEventListener("DOMContentLoaded",()=>{
    for(const id of ["limit-events","limit-periods"]){$(id).checked=false;$(id).onchange=()=>{if(datasets.size)renderOverview();};}
    // The peak label can cover overlay text, so it can be hidden; the choice is remembered.
    try{$("show-peak").checked=localStorage.getItem("showPeak")!=="false";}catch(_){$("show-peak").checked=true;}
    $("show-peak").onchange=()=>{try{localStorage.setItem("showPeak",String($("show-peak").checked));}catch(_){}if(datasets.size)renderOverview();};
    for(const id of ["agent","host","project"])$(id).addEventListener("change",fitScopes);
    document.fonts.ready.then(fitScopes);
    theme();$("theme").onclick=()=>{const t=document.documentElement.dataset.tmTheme;theme(THEMES[(THEMES.indexOf(t)+1)%THEMES.length]);};
    $("export-all").onclick=()=>copyCharts($("export-all"),[...shareSpecs,...Object.values(modelViews).map(v=>v.spec).filter(Boolean),...[...widgets.values()].map(w=>w.spec)],true);
    $("model-pick").onchange=event=>{modelKey=event.target.value;try{localStorage.setItem("model",modelKey);}catch(_){}renderModelTokens();};
    for(const v of Object.values(modelViews)){
      const card=$(v.id),toggle=card.querySelector(".toggle"),copy=card.querySelector(".copy");
      toggle.onclick=()=>{v.table=!v.table;toggle.setAttribute("aria-pressed",String(v.table));if(v===modelViews.tokens)renderModelTokens();else if(v===modelViews.hit)renderModelHit();else renderEffort();if(!v.table)v.chart?.resize();};
      copy.onclick=()=>{if(v.spec)copyCharts(copy,[v.spec]);};
    }
    for(const b of document.querySelectorAll("[data-scope-by]"))b.onclick=()=>{scopeBy=b.dataset.scopeBy;try{localStorage.setItem("scopeBy",scopeBy);}catch(_){}if(datasets.size)renderOverview();};
    $("trend-chips").onclick=event=>{if(event.target.closest("#brush-clear"))clearBrush();};
    document.addEventListener("keydown",event=>{if(event.key==="Escape")clearBrush();});
    for(const b of document.querySelectorAll("[data-share-metric]"))b.onclick=()=>{shareMetric=b.dataset.shareMetric;try{localStorage.setItem("shareMetric",shareMetric);}catch(_){}if(datasets.size)renderShare();};
    for(const b of document.querySelectorAll("[data-metric]"))b.onclick=()=>{metric=b.dataset.metric;try{localStorage.setItem("metric",metric);}catch(_){}if(datasets.size)renderOverview();};
    $("agent-region").onclick=event=>{
      const tab=event.target.closest("[data-tab]"),goto=event.target.closest("[data-goto]");
      if(tab){agentTab=tab.dataset.tab;try{localStorage.setItem("agentTab",agentTab);}catch(_){}renderOverview();$("agent-title").querySelector(`[data-tab="${agentTab}"]`)?.focus();}
      if(goto)$("range-"+goto.dataset.goto).click();
    };
    // Arrow keys move between agent tabs (roving tabindex).
    $("agent-title").onkeydown=event=>{
      const tabs=[...$("agent-title").querySelectorAll("[data-tab]")],i=tabs.indexOf(event.target.closest("[data-tab]"));
      if(i<0||!["ArrowLeft","ArrowRight"].includes(event.key))return;
      event.preventDefault();tabs[(i+(event.key==="ArrowRight"?1:tabs.length-1))%tabs.length].click();
    };
    // Raw charts are drawn while the section is closed, so they are resized when it opens.
    $("raw").ontoggle=()=>{if($("raw").open){widgets.forEach(w=>w.chart?.resize());balanceAll();}};
    setInterval(()=>{if(lastPacket&&Date.now()-lastPacket>75000){$("updated-mark").dataset.state="pending";condition("connection","warning","Connection lost","Live connection interrupted. Reconnecting…");}},15000);
  });
  window.addEventListener("pagehide",()=>widgets.forEach(w=>w.chart?.dispose()));
})();
