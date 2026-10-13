// Source-aligned, side-effect-free wrapper around Claude Code 2.1.295 Ft.
// Renderer slice: source local bytes [47013, 93227), SHA-256 recorded in manifest.
import { Rs } from "./insights_html_native_escape.mjs";
var Qe={debug_investigate:"Debug/Investigate",implement_feature:"Implement Feature",fix_bug:"Fix Bug",write_script_tool:"Write Script/Tool",refactor_code:"Refactor Code",configure_system:"Configure System",create_pr_commit:"Create PR/Commit",analyze_data:"Analyze Data",understand_codebase:"Understand Codebase",write_tests:"Write Tests",write_docs:"Write Docs",deploy_infra:"Deploy/Infra",warmup_minimal:"Cache Warmup",fast_accurate_search:"Fast/Accurate Search",correct_code_edits:"Correct Code Edits",good_explanations:"Good Explanations",proactive_help:"Proactive Help",multi_file_changes:"Multi-file Changes",handled_complexity:"Multi-file Changes",good_debugging:"Good Debugging",misunderstood_request:"Misunderstood Request",wrong_approach:"Wrong Approach",buggy_code:"Buggy Code",user_rejected_action:"User Rejected Action",claude_got_blocked:"LingXi Got Blocked",user_stopped_early:"User Stopped Early",wrong_file_or_location:"Wrong File/Location",excessive_changes:"Excessive Changes",slow_or_verbose:"Slow/Verbose",tool_failed:"Tool Failed",user_unclear:"User Unclear",external_issue:"External Issue",frustrated:"Frustrated",dissatisfied:"Dissatisfied",likely_satisfied:"Likely Satisfied",satisfied:"Satisfied",happy:"Happy",unsure:"Unsure",neutral:"Neutral",delighted:"Delighted",single_task:"Single Task",multi_task:"Multi Task",iterative_refinement:"Iterative Refinement",exploration:"Exploration",quick_question:"Quick Question",fully_achieved:"Fully Achieved",mostly_achieved:"Mostly Achieved",partially_achieved:"Partially Achieved",not_achieved:"Not Achieved",unclear_from_transcript:"Unclear",unhelpful:"Unhelpful",slightly_helpful:"Slightly Helpful",moderately_helpful:"Moderately Helpful",very_helpful:"Very Helpful",essential:"Essential"};
function We(e){if(e<100)return e;let s=10**(Math.floor(Math.log10(e))-1);return Math.round(e/s)*s}
// Native chunk-7zg77ry0 export `_` is JSON.stringify(e, replacer, space);
// Pe(e) is used only for source instrumentation and does not transform e.
function _(value, replacer, space) { return JSON.stringify(value, replacer, space); }

function fe(e){return Rs(e).replace(/\*\*(.+?)\*\*/g,"<strong>$1</strong>")}var Pt=["frustrated","dissatisfied","likely_satisfied","satisfied","happy","unsure"],jt=["not_achieved","partially_achieved","mostly_achieved","fully_achieved","unclear_from_transcript"];function ee(e,s,n=6,a){let l;if(a)l=a.filter((g)=>(g in e)&&(e[g]??0)>0).map((g)=>[g,e[g]??0]);else l=Object.entries(e).sort((g,y)=>y[1]-g[1]).slice(0,n);if(l.length===0)return'<p class="empty">No data</p>';let m=Math.max(...l.map((g)=>g[1]));return l.map(([g,y])=>{let w=y/m*100,r=Qe[g]||g.replaceAll("_"," ").replace(/\b\w/g,(f)=>f.toUpperCase());return`<div class="bar-row">
        <div class="bar-label">${Rs(r)}</div>
        <div class="bar-track"><div class="bar-fill" style="width:${w}%;background:${s}"></div></div>
        <div class="bar-value">${y}</div>
      </div>`}).join(`
`)}function Dt(e){if(e.length===0)return'<p class="empty">No response time data</p>';let s={"2-10s":0,"10-30s":0,"30s-1m":0,"1-2m":0,"2-5m":0,"5-15m":0,">15m":0};for(let a of e)if(a<10)s["2-10s"]=(s["2-10s"]??0)+1;else if(a<30)s["10-30s"]=(s["10-30s"]??0)+1;else if(a<60)s["30s-1m"]=(s["30s-1m"]??0)+1;else if(a<120)s["1-2m"]=(s["1-2m"]??0)+1;else if(a<300)s["2-5m"]=(s["2-5m"]??0)+1;else if(a<900)s["5-15m"]=(s["5-15m"]??0)+1;else s[">15m"]=(s[">15m"]??0)+1;let n=Math.max(...Object.values(s));if(n===0)return'<p class="empty">No response time data</p>';return Object.entries(s).map(([a,l])=>{let m=l/n*100;return`<div class="bar-row">
        <div class="bar-label">${a}</div>
        <div class="bar-track"><div class="bar-fill" style="width:${m}%;background:#6366f1"></div></div>
        <div class="bar-value">${l}</div>
      </div>`}).join(`
`)}function Nt(e){if(e.length===0)return'<p class="empty">No time data</p>';let s=[{label:"Morning (6-12)",range:[6,7,8,9,10,11]},{label:"Afternoon (12-18)",range:[12,13,14,15,16,17]},{label:"Evening (18-24)",range:[18,19,20,21,22,23]},{label:"Night (0-6)",range:[0,1,2,3,4,5]}],n={};for(let g of e)n[g]=(n[g]||0)+1;let a=s.map((g)=>({label:g.label,count:g.range.reduce((y,w)=>y+(n[w]||0),0)})),l=Math.max(...a.map((g)=>g.count))||1;return`<div id="hour-histogram">${a.map((g)=>`
      <div class="bar-row">
        <div class="bar-label">${g.label}</div>
        <div class="bar-track"><div class="bar-fill" style="width:${g.count/l*100}%;background:#8b5cf6"></div></div>
        <div class="bar-value">${g.count}</div>
      </div>`).join(`
`)}</div>`}function Lt(e){let s={};for(let n of e)s[n]=(s[n]||0)+1;return _(s)}function Ft(e,s,n){let a=(b)=>{if(!b)return"";return b.split(`

`).map((A)=>{let E=Rs(A);return E=E.replace(/\*\*(.+?)\*\*/g,"<strong>$1</strong>"),E=E.replace(/^- /gm,"\u2022 "),E=E.replaceAll(`
`,"<br>"),`<p>${E}</p>`}).join(`
`)},l=s.at_a_glance,m=l?`
    <div class="at-a-glance">
      <div class="glance-title">At a Glance</div>
      <div class="glance-sections">
        ${l.whats_working?`<div class="glance-section"><strong>What's working:</strong> ${fe(l.whats_working)} <a href="#section-wins" class="see-more">Impressive Things You Did \u2192</a></div>`:""}
        ${l.whats_hindering?`<div class="glance-section"><strong>What's hindering you:</strong> ${fe(l.whats_hindering)} <a href="#section-friction" class="see-more">Where Things Go Wrong \u2192</a></div>`:""}
        ${l.quick_wins?`<div class="glance-section"><strong>Quick wins to try:</strong> ${fe(l.quick_wins)} <a href="#section-features" class="see-more">Features to Try \u2192</a></div>`:""}
        ${l.ambitious_workflows?`<div class="glance-section"><strong>Ambitious workflows:</strong> ${fe(l.ambitious_workflows)} <a href="#section-horizon" class="see-more">On the Horizon \u2192</a></div>`:""}
      </div>
    </div>
    `:"",g=!n?"":n.kind==="switch_to_auto"?`
    <div class="auto-mode-card" id="section-auto-mode">
      <div class="auto-mode-title">Auto mode could have handled up to ~${We(n.estimatedPrompts).toLocaleString()} permission prompts for you</div>
      <div class="auto-mode-desc">Across ${n.nonAutoSessions.toLocaleString()} recent sessions, roughly that many tool calls fell outside LingXi's always-safe set \u2014 each one a potential "can I run this?" interruption. Auto mode approves routine actions automatically and only checks in on risky ones. Press <code>Shift+Tab</code> in any session until the mode indicator shows auto.</div>
      ${n.destructiveCommands>0?`<div class="auto-mode-note">It stays careful: ${n.destructiveCommands.toLocaleString()} of those commands looked potentially destructive (force-deletes, history rewrites, and the like) \u2014 auto mode reviews actions like these instead of waving them through.</div>`:""}
      <div class="auto-mode-fineprint">Estimated from your local session history; the actual number depends on your allowlist rules.</div>
    </div>
    `:`
    <div class="auto-mode-card" id="section-auto-mode">
      <div class="auto-mode-title">Make auto mode smarter: run /auto-mode-setup</div>
      <div class="auto-mode-desc">You already use auto mode, but it doesn't know your environment yet. <code>/auto-mode-setup</code> spends a few minutes mapping your repos, trusted services, and routine commands, then writes the result to your settings \u2014 auto mode's decisions get noticeably better. One approval at the end, and you can re-run it anytime.</div>
    </div>
    `,y=s.project_areas?.areas||[],w=y.length>0?`
    <h2 id="section-work">What You Work On</h2>
    <div class="project-areas">
      ${y.map((b)=>`
        <div class="project-area">
          <div class="area-header">
            <span class="area-name">${Rs(b.name)}</span>
            <span class="area-count">~${b.session_count} sessions</span>
          </div>
          <div class="area-desc">${Rs(b.description)}</div>
        </div>
      `).join("")}
    </div>
    `:"",r=s.interaction_style,f=r?.narrative?`
    <h2 id="section-usage">How You Use LingXi</h2>
    <div class="narrative">
      ${a(r.narrative)}
      ${r.key_pattern?`<div class="key-insight"><strong>Key pattern:</strong> ${Rs(r.key_pattern)}</div>`:""}
    </div>
    `:"",h=s.what_works,T=h?.impressive_workflows&&h.impressive_workflows.length>0?`
    <h2 id="section-wins">Impressive Things You Did</h2>
    ${h.intro?`<p class="section-intro">${Rs(h.intro)}</p>`:""}
    <div class="big-wins">
      ${h.impressive_workflows.map((b)=>`
        <div class="big-win">
          <div class="big-win-title">${Rs(b.title||"")}</div>
          <div class="big-win-desc">${Rs(b.description||"")}</div>
        </div>
      `).join("")}
    </div>
    `:"",R=s.friction_analysis,I=R?.categories&&R.categories.length>0?`
    <h2 id="section-friction">Where Things Go Wrong</h2>
    ${R.intro?`<p class="section-intro">${Rs(R.intro)}</p>`:""}
    <div class="friction-categories">
      ${R.categories.map((b)=>`
        <div class="friction-category">
          <div class="friction-title">${Rs(b.category||"")}</div>
          <div class="friction-desc">${Rs(b.description||"")}</div>
          ${b.examples?`<ul class="friction-examples">${b.examples.map((A)=>`<li>${Rs(A)}</li>`).join("")}</ul>`:""}
        </div>
      `).join("")}
    </div>
    `:"",O=s.suggestions,K=O?`
    ${O.claude_md_additions&&O.claude_md_additions.length>0?`
    <h2 id="section-features">Existing LingXi Features to Try</h2>
    <div class="claude-md-section">
      <h3>Suggested LINGXI.md Additions</h3>
      <p style="font-size: 12px; color: #64748b; margin-bottom: 12px;">Just copy this into LingXi to add it to your LINGXI.md.</p>
      <div class="claude-md-actions">
        <button class="copy-all-btn" onclick="copyAllCheckedClaudeMd()">Copy All Checked</button>
      </div>
      ${O.claude_md_additions.map((b,A)=>`
        <div class="claude-md-item">
          <input type="checkbox" id="cmd-${A}" class="cmd-checkbox" checked data-text="${Rs(b.prompt_scaffold||b.where||"Add to LINGXI.md")}\\n\\n${Rs(b.addition)}">
          <label for="cmd-${A}">
            <code class="cmd-code">${Rs(b.addition)}</code>
            <button class="copy-btn" onclick="copyCmdItem(${A})">Copy</button>
          </label>
          <div class="cmd-why">${Rs(b.why)}</div>
        </div>
      `).join("")}
    </div>
    `:""}
    ${O.features_to_try&&O.features_to_try.length>0?`
    <p style="font-size: 13px; color: #64748b; margin-bottom: 12px;">Just copy this into LingXi and it'll set it up for you.</p>
    <div class="features-section">
      ${O.features_to_try.map((b)=>`
        <div class="feature-card">
          <div class="feature-title">${Rs(b.feature||"")}</div>
          <div class="feature-oneliner">${Rs(b.one_liner||"")}</div>
          <div class="feature-why"><strong>Why for you:</strong> ${Rs(b.why_for_you||"")}</div>
          ${b.example_code?`
          <div class="feature-examples">
            <div class="feature-example">
              <div class="example-code-row">
                <code class="example-code">${Rs(b.example_code)}</code>
                <button class="copy-btn" onclick="copyText(this)">Copy</button>
              </div>
            </div>
          </div>
          `:""}
        </div>
      `).join("")}
    </div>
    `:""}
    ${O.usage_patterns&&O.usage_patterns.length>0?`
    <h2 id="section-patterns">New Ways to Use LingXi</h2>
    <p style="font-size: 13px; color: #64748b; margin-bottom: 12px;">Just copy this into LingXi and it'll walk you through it.</p>
    <div class="patterns-section">
      ${O.usage_patterns.map((b)=>`
        <div class="pattern-card">
          <div class="pattern-title">${Rs(b.title||"")}</div>
          <div class="pattern-summary">${Rs(b.suggestion||"")}</div>
          ${b.detail?`<div class="pattern-detail">${Rs(b.detail)}</div>`:""}
          ${b.copyable_prompt?`
          <div class="copyable-prompt-section">
            <div class="prompt-label">Paste into LingXi:</div>
            <div class="copyable-prompt-row">
              <code class="copyable-prompt">${Rs(b.copyable_prompt)}</code>
              <button class="copy-btn" onclick="copyText(this)">Copy</button>
            </div>
          </div>
          `:""}
        </div>
      `).join("")}
    </div>
    `:""}
    `:"",z=s.on_the_horizon,W=z?.opportunities&&z.opportunities.length>0?`
    <h2 id="section-horizon">On the Horizon</h2>
    ${z.intro?`<p class="section-intro">${Rs(z.intro)}</p>`:""}
    <div class="horizon-section">
      ${z.opportunities.map((b)=>`
        <div class="horizon-card">
          <div class="horizon-title">${Rs(b.title||"")}</div>
          <div class="horizon-possible">${Rs(b.whats_possible||"")}</div>
          ${b.how_to_try?`<div class="horizon-tip"><strong>Getting started:</strong> ${Rs(b.how_to_try)}</div>`:""}
          ${b.copyable_prompt?`<div class="pattern-prompt"><div class="prompt-label">Paste into LingXi:</div><code>${Rs(b.copyable_prompt)}</code><button class="copy-btn" onclick="copyText(this)">Copy</button></div>`:""}
        </div>
      `).join("")}
    </div>
    `:"",S=[],L=[],oe=S.length>0||L.length>0?`
    <h2 id="section-feedback" class="feedback-header">Closing the Loop: Feedback for Other Teams</h2>
    <p class="feedback-intro">Suggestions for the CC product and model teams based on your usage patterns. Click to expand.</p>
    ${S.length>0?`
    <div class="collapsible-section">
      <div class="collapsible-header" onclick="toggleCollapsible(this)">
        <span class="collapsible-arrow">\u25B6</span>
        <h3>Product Improvements for CC Team</h3>
      </div>
      <div class="collapsible-content">
        <div class="suggestions-section">
          ${S.map((b)=>`
            <div class="feedback-card team-card">
              <div class="feedback-title">${Rs(b.title||"")}</div>
              <div class="feedback-detail">${Rs(b.detail||"")}</div>
              ${b.evidence?`<div class="feedback-evidence"><em>Evidence:</em> ${Rs(b.evidence)}</div>`:""}
            </div>
          `).join("")}
        </div>
      </div>
    </div>
    `:""}
    ${L.length>0?`
    <div class="collapsible-section">
      <div class="collapsible-header" onclick="toggleCollapsible(this)">
        <span class="collapsible-arrow">\u25B6</span>
        <h3>Model Behavior Improvements</h3>
      </div>
      <div class="collapsible-content">
        <div class="suggestions-section">
          ${L.map((b)=>`
            <div class="feedback-card model-card">
              <div class="feedback-title">${Rs(b.title||"")}</div>
              <div class="feedback-detail">${Rs(b.detail||"")}</div>
              ${b.evidence?`<div class="feedback-evidence"><em>Evidence:</em> ${Rs(b.evidence)}</div>`:""}
            </div>
          `).join("")}
        </div>
      </div>
    </div>
    `:""}
    `:"",Z=s.fun_ending,ie=Z?.headline?`
    <div class="fun-ending">
      <div class="fun-headline">"${Rs(Z.headline)}"</div>
      ${Z.detail?`<div class="fun-detail">${Rs(Z.detail)}</div>`:""}
    </div>
    `:"",J=`
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body { font-family: 'Inter', -apple-system, BlinkMacSystemFont, sans-serif; background: #f8fafc; color: #334155; line-height: 1.65; padding: 48px 24px; }
    .container { max-width: 800px; margin: 0 auto; }
    h1 { font-size: 32px; font-weight: 700; color: #0f172a; margin-bottom: 8px; }
    h2 { font-size: 20px; font-weight: 600; color: #0f172a; margin-top: 48px; margin-bottom: 16px; }
    .subtitle { color: #64748b; font-size: 15px; margin-bottom: 32px; }
    .nav-toc { display: flex; flex-wrap: wrap; gap: 8px; margin: 24px 0 32px 0; padding: 16px; background: white; border-radius: 8px; border: 1px solid #e2e8f0; }
    .nav-toc a { font-size: 12px; color: #64748b; text-decoration: none; padding: 6px 12px; border-radius: 6px; background: #f1f5f9; transition: all 0.15s; }
    .nav-toc a:hover { background: #e2e8f0; color: #334155; }
    .stats-row { display: flex; gap: 24px; margin-bottom: 40px; padding: 20px 0; border-top: 1px solid #e2e8f0; border-bottom: 1px solid #e2e8f0; flex-wrap: wrap; }
    .stat { text-align: center; }
    .stat-value { font-size: 24px; font-weight: 700; color: #0f172a; }
    .stat-label { font-size: 11px; color: #64748b; text-transform: uppercase; }
    .at-a-glance { background: linear-gradient(135deg, #fef3c7 0%, #fde68a 100%); border: 1px solid #f59e0b; border-radius: 12px; padding: 20px 24px; margin-bottom: 32px; }
    .glance-title { font-size: 16px; font-weight: 700; color: #92400e; margin-bottom: 16px; }
    .glance-sections { display: flex; flex-direction: column; gap: 12px; }
    .glance-section { font-size: 14px; color: #78350f; line-height: 1.6; }
    .glance-section strong { color: #92400e; }
    .see-more { color: #b45309; text-decoration: none; font-size: 13px; white-space: nowrap; }
    .see-more:hover { text-decoration: underline; }
    .auto-mode-card { background: linear-gradient(135deg, #eff6ff 0%, #e0f2fe 100%); border: 1px solid #60a5fa; border-radius: 12px; padding: 20px 24px; margin-bottom: 32px; }
    .auto-mode-title { font-size: 16px; font-weight: 700; color: #1e40af; margin-bottom: 10px; }
    .auto-mode-desc { font-size: 14px; color: #1e3a8a; line-height: 1.6; }
    .auto-mode-desc code { background: rgba(255,255,255,0.7); padding: 1px 6px; border-radius: 4px; font-size: 13px; }
    .auto-mode-note { font-size: 13px; color: #1e3a8a; background: rgba(255,255,255,0.6); padding: 8px 12px; border-radius: 6px; margin-top: 10px; }
    .auto-mode-fineprint { font-size: 11px; color: #3b82f6; margin-top: 10px; }
    .project-areas { display: flex; flex-direction: column; gap: 12px; margin-bottom: 32px; }
    .project-area { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; }
    .area-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; }
    .area-name { font-weight: 600; font-size: 15px; color: #0f172a; }
    .area-count { font-size: 12px; color: #64748b; background: #f1f5f9; padding: 2px 8px; border-radius: 4px; }
    .area-desc { font-size: 14px; color: #475569; line-height: 1.5; }
    .narrative { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 20px; margin-bottom: 24px; }
    .narrative p { margin-bottom: 12px; font-size: 14px; color: #475569; line-height: 1.7; }
    .key-insight { background: #f0fdf4; border: 1px solid #bbf7d0; border-radius: 8px; padding: 12px 16px; margin-top: 12px; font-size: 14px; color: #166534; }
    .section-intro { font-size: 14px; color: #64748b; margin-bottom: 16px; }
    .big-wins { display: flex; flex-direction: column; gap: 12px; margin-bottom: 24px; }
    .big-win { background: #f0fdf4; border: 1px solid #bbf7d0; border-radius: 8px; padding: 16px; }
    .big-win-title { font-weight: 600; font-size: 15px; color: #166534; margin-bottom: 8px; }
    .big-win-desc { font-size: 14px; color: #15803d; line-height: 1.5; }
    .friction-categories { display: flex; flex-direction: column; gap: 16px; margin-bottom: 24px; }
    .friction-category { background: #fef2f2; border: 1px solid #fca5a5; border-radius: 8px; padding: 16px; }
    .friction-title { font-weight: 600; font-size: 15px; color: #991b1b; margin-bottom: 6px; }
    .friction-desc { font-size: 13px; color: #7f1d1d; margin-bottom: 10px; }
    .friction-examples { margin: 0 0 0 20px; font-size: 13px; color: #334155; }
    .friction-examples li { margin-bottom: 4px; }
    .claude-md-section { background: #eff6ff; border: 1px solid #bfdbfe; border-radius: 8px; padding: 16px; margin-bottom: 20px; }
    .claude-md-section h3 { font-size: 14px; font-weight: 600; color: #1e40af; margin: 0 0 12px 0; }
    .claude-md-actions { margin-bottom: 12px; padding-bottom: 12px; border-bottom: 1px solid #dbeafe; }
    .copy-all-btn { background: #2563eb; color: white; border: none; border-radius: 4px; padding: 6px 12px; font-size: 12px; cursor: pointer; font-weight: 500; transition: all 0.2s; }
    .copy-all-btn:hover { background: #1d4ed8; }
    .copy-all-btn.copied { background: #16a34a; }
    .claude-md-item { display: flex; flex-wrap: wrap; align-items: flex-start; gap: 8px; padding: 10px 0; border-bottom: 1px solid #dbeafe; }
    .claude-md-item:last-child { border-bottom: none; }
    .cmd-checkbox { margin-top: 2px; }
    .cmd-code { background: white; padding: 8px 12px; border-radius: 4px; font-size: 12px; color: #1e40af; border: 1px solid #bfdbfe; font-family: monospace; display: block; white-space: pre-wrap; word-break: break-word; flex: 1; }
    .cmd-why { font-size: 12px; color: #64748b; width: 100%; padding-left: 24px; margin-top: 4px; }
    .features-section, .patterns-section { display: flex; flex-direction: column; gap: 12px; margin: 16px 0; }
    .feature-card { background: #f0fdf4; border: 1px solid #86efac; border-radius: 8px; padding: 16px; }
    .pattern-card { background: #f0f9ff; border: 1px solid #7dd3fc; border-radius: 8px; padding: 16px; }
    .feature-title, .pattern-title { font-weight: 600; font-size: 15px; color: #0f172a; margin-bottom: 6px; }
    .feature-oneliner { font-size: 14px; color: #475569; margin-bottom: 8px; }
    .pattern-summary { font-size: 14px; color: #475569; margin-bottom: 8px; }
    .feature-why, .pattern-detail { font-size: 13px; color: #334155; line-height: 1.5; }
    .feature-examples { margin-top: 12px; }
    .feature-example { padding: 8px 0; border-top: 1px solid #d1fae5; }
    .feature-example:first-child { border-top: none; }
    .example-desc { font-size: 13px; color: #334155; margin-bottom: 6px; }
    .example-code-row { display: flex; align-items: flex-start; gap: 8px; }
    .example-code { flex: 1; background: #f1f5f9; padding: 8px 12px; border-radius: 4px; font-family: monospace; font-size: 12px; color: #334155; overflow-x: auto; white-space: pre-wrap; }
    .copyable-prompt-section { margin-top: 12px; padding-top: 12px; border-top: 1px solid #e2e8f0; }
    .copyable-prompt-row { display: flex; align-items: flex-start; gap: 8px; }
    .copyable-prompt { flex: 1; background: #f8fafc; padding: 10px 12px; border-radius: 4px; font-family: monospace; font-size: 12px; color: #334155; border: 1px solid #e2e8f0; white-space: pre-wrap; line-height: 1.5; }
    .feature-code { background: #f8fafc; padding: 12px; border-radius: 6px; margin-top: 12px; border: 1px solid #e2e8f0; display: flex; align-items: flex-start; gap: 8px; }
    .feature-code code { flex: 1; font-family: monospace; font-size: 12px; color: #334155; white-space: pre-wrap; }
    .pattern-prompt { background: #f8fafc; padding: 12px; border-radius: 6px; margin-top: 12px; border: 1px solid #e2e8f0; }
    .pattern-prompt code { font-family: monospace; font-size: 12px; color: #334155; display: block; white-space: pre-wrap; margin-bottom: 8px; }
    .prompt-label { font-size: 11px; font-weight: 600; text-transform: uppercase; color: #64748b; margin-bottom: 6px; }
    .copy-btn { background: #e2e8f0; border: none; border-radius: 4px; padding: 4px 8px; font-size: 11px; cursor: pointer; color: #475569; flex-shrink: 0; }
    .copy-btn:hover { background: #cbd5e1; }
    .charts-row { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; margin: 24px 0; }
    .chart-card { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; }
    .chart-title { font-size: 12px; font-weight: 600; color: #64748b; text-transform: uppercase; margin-bottom: 12px; }
    .bar-row { display: flex; align-items: center; margin-bottom: 6px; }
    .bar-label { width: 100px; font-size: 11px; color: #475569; flex-shrink: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .bar-track { flex: 1; height: 6px; background: #f1f5f9; border-radius: 3px; margin: 0 8px; }
    .bar-fill { height: 100%; border-radius: 3px; }
    .bar-value { width: 28px; font-size: 11px; font-weight: 500; color: #64748b; text-align: right; }
    .empty { color: #94a3b8; font-size: 13px; }
    .horizon-section { display: flex; flex-direction: column; gap: 16px; }
    .horizon-card { background: linear-gradient(135deg, #faf5ff 0%, #f5f3ff 100%); border: 1px solid #c4b5fd; border-radius: 8px; padding: 16px; }
    .horizon-title { font-weight: 600; font-size: 15px; color: #5b21b6; margin-bottom: 8px; }
    .horizon-possible { font-size: 14px; color: #334155; margin-bottom: 10px; line-height: 1.5; }
    .horizon-tip { font-size: 13px; color: #6b21a8; background: rgba(255,255,255,0.6); padding: 8px 12px; border-radius: 4px; }
    .feedback-header { margin-top: 48px; color: #64748b; font-size: 16px; }
    .feedback-intro { font-size: 13px; color: #94a3b8; margin-bottom: 16px; }
    .feedback-section { margin-top: 16px; }
    .feedback-section h3 { font-size: 14px; font-weight: 600; color: #475569; margin-bottom: 12px; }
    .feedback-card { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; margin-bottom: 12px; }
    .feedback-card.team-card { background: #eff6ff; border-color: #bfdbfe; }
    .feedback-card.model-card { background: #faf5ff; border-color: #e9d5ff; }
    .feedback-title { font-weight: 600; font-size: 14px; color: #0f172a; margin-bottom: 6px; }
    .feedback-detail { font-size: 13px; color: #475569; line-height: 1.5; }
    .feedback-evidence { font-size: 12px; color: #64748b; margin-top: 8px; }
    .fun-ending { background: linear-gradient(135deg, #fef3c7 0%, #fde68a 100%); border: 1px solid #fbbf24; border-radius: 12px; padding: 24px; margin-top: 40px; text-align: center; }
    .fun-headline { font-size: 18px; font-weight: 600; color: #78350f; margin-bottom: 8px; }
    .fun-detail { font-size: 14px; color: #92400e; }
    .collapsible-section { margin-top: 16px; }
    .collapsible-header { display: flex; align-items: center; gap: 8px; cursor: pointer; padding: 12px 0; border-bottom: 1px solid #e2e8f0; }
    .collapsible-header h3 { margin: 0; font-size: 14px; font-weight: 600; color: #475569; }
    .collapsible-arrow { font-size: 12px; color: #94a3b8; transition: transform 0.2s; }
    .collapsible-content { display: none; padding-top: 16px; }
    .collapsible-content.open { display: block; }
    .collapsible-header.open .collapsible-arrow { transform: rotate(90deg); }
    @media (max-width: 640px) { .charts-row { grid-template-columns: 1fr; } .stats-row { justify-content: center; } }
  `,D=`
    function toggleCollapsible(header) {
      header.classList.toggle('open');
      const content = header.nextElementSibling;
      content.classList.toggle('open');
    }
    function copyText(btn) {
      const code = btn.previousElementSibling;
      navigator.clipboard.writeText(code.textContent).then(() => {
        btn.textContent = 'Copied!';
        setTimeout(() => { btn.textContent = 'Copy'; }, 2000);
      });
    }
    function copyCmdItem(idx) {
      const checkbox = document.getElementById('cmd-' + idx);
      if (checkbox) {
        const text = checkbox.dataset.text;
        navigator.clipboard.writeText(text).then(() => {
          const btn = checkbox.nextElementSibling.querySelector('.copy-btn');
          if (btn) { btn.textContent = 'Copied!'; setTimeout(() => { btn.textContent = 'Copy'; }, 2000); }
        });
      }
    }
    function copyAllCheckedClaudeMd() {
      const checkboxes = document.querySelectorAll('.cmd-checkbox:checked');
      const texts = [];
      checkboxes.forEach(cb => {
        if (cb.dataset.text) { texts.push(cb.dataset.text); }
      });
      const combined = texts.join('\\n');
      const btn = document.querySelector('.copy-all-btn');
      if (btn) {
        navigator.clipboard.writeText(combined).then(() => {
          btn.textContent = 'Copied ' + texts.length + ' items!';
          btn.classList.add('copied');
          setTimeout(() => { btn.textContent = 'Copy All Checked'; btn.classList.remove('copied'); }, 2000);
        });
      }
    }
    // Timezone selector for time of day chart (data is from our own analytics, not user input)
    const rawHourCounts = ${Lt(e.message_hours)};
    function updateHourHistogram(offsetFromPT) {
      const periods = [
        { label: "Morning (6-12)", range: [6,7,8,9,10,11] },
        { label: "Afternoon (12-18)", range: [12,13,14,15,16,17] },
        { label: "Evening (18-24)", range: [18,19,20,21,22,23] },
        { label: "Night (0-6)", range: [0,1,2,3,4,5] }
      ];
      const adjustedCounts = {};
      for (const [hour, count] of Object.entries(rawHourCounts)) {
        const newHour = (parseInt(hour) + offsetFromPT + 24) % 24;
        adjustedCounts[newHour] = (adjustedCounts[newHour] || 0) + count;
      }
      const periodCounts = periods.map(p => ({
        label: p.label,
        count: p.range.reduce((sum, h) => sum + (adjustedCounts[h] || 0), 0)
      }));
      const maxCount = Math.max(...periodCounts.map(p => p.count)) || 1;
      const container = document.getElementById('hour-histogram');
      container.textContent = '';
      periodCounts.forEach(p => {
        const row = document.createElement('div');
        row.className = 'bar-row';
        const label = document.createElement('div');
        label.className = 'bar-label';
        label.textContent = p.label;
        const track = document.createElement('div');
        track.className = 'bar-track';
        const fill = document.createElement('div');
        fill.className = 'bar-fill';
        fill.style.width = (p.count / maxCount) * 100 + '%';
        fill.style.background = '#8b5cf6';
        track.appendChild(fill);
        const value = document.createElement('div');
        value.className = 'bar-value';
        value.textContent = p.count;
        row.appendChild(label);
        row.appendChild(track);
        row.appendChild(value);
        container.appendChild(row);
      });
    }
    document.getElementById('timezone-select').addEventListener('change', function() {
      const customInput = document.getElementById('custom-offset');
      if (this.value === 'custom') {
        customInput.style.display = 'inline-block';
        customInput.focus();
      } else {
        customInput.style.display = 'none';
        updateHourHistogram(parseInt(this.value));
      }
    });
    document.getElementById('custom-offset').addEventListener('change', function() {
      const parsed = parseInt(this.value, 10);
      if (isNaN(parsed)) return;
      updateHourHistogram(parsed + 8);
    });
  `;return`<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <title>LingXi Insights</title>
  <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&display=swap" rel="stylesheet">
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body { font-family: 'Inter', -apple-system, BlinkMacSystemFont, sans-serif; background: #f8fafc; color: #334155; line-height: 1.65; padding: 48px 24px; }
    .container { max-width: 800px; margin: 0 auto; }
    h1 { font-size: 32px; font-weight: 700; color: #0f172a; margin-bottom: 8px; }
    h2 { font-size: 20px; font-weight: 600; color: #0f172a; margin-top: 48px; margin-bottom: 16px; }
    .subtitle { color: #64748b; font-size: 15px; margin-bottom: 32px; }
    .nav-toc { display: flex; flex-wrap: wrap; gap: 8px; margin: 24px 0 32px 0; padding: 16px; background: white; border-radius: 8px; border: 1px solid #e2e8f0; }
    .nav-toc a { font-size: 12px; color: #64748b; text-decoration: none; padding: 6px 12px; border-radius: 6px; background: #f1f5f9; transition: all 0.15s; }
    .nav-toc a:hover { background: #e2e8f0; color: #334155; }
    .stats-row { display: flex; gap: 24px; margin-bottom: 40px; padding: 20px 0; border-top: 1px solid #e2e8f0; border-bottom: 1px solid #e2e8f0; flex-wrap: wrap; }
    .stat { text-align: center; }
    .stat-value { font-size: 24px; font-weight: 700; color: #0f172a; }
    .stat-label { font-size: 11px; color: #64748b; text-transform: uppercase; }
    .at-a-glance { background: linear-gradient(135deg, #fef3c7 0%, #fde68a 100%); border: 1px solid #f59e0b; border-radius: 12px; padding: 20px 24px; margin-bottom: 32px; }
    .glance-title { font-size: 16px; font-weight: 700; color: #92400e; margin-bottom: 16px; }
    .glance-sections { display: flex; flex-direction: column; gap: 12px; }
    .glance-section { font-size: 14px; color: #78350f; line-height: 1.6; }
    .glance-section strong { color: #92400e; }
    .see-more { color: #b45309; text-decoration: none; font-size: 13px; white-space: nowrap; }
    .see-more:hover { text-decoration: underline; }
    .auto-mode-card { background: linear-gradient(135deg, #eff6ff 0%, #e0f2fe 100%); border: 1px solid #60a5fa; border-radius: 12px; padding: 20px 24px; margin-bottom: 32px; }
    .auto-mode-title { font-size: 16px; font-weight: 700; color: #1e40af; margin-bottom: 10px; }
    .auto-mode-desc { font-size: 14px; color: #1e3a8a; line-height: 1.6; }
    .auto-mode-desc code { background: rgba(255,255,255,0.7); padding: 1px 6px; border-radius: 4px; font-size: 13px; }
    .auto-mode-note { font-size: 13px; color: #1e3a8a; background: rgba(255,255,255,0.6); padding: 8px 12px; border-radius: 6px; margin-top: 10px; }
    .auto-mode-fineprint { font-size: 11px; color: #3b82f6; margin-top: 10px; }
    .project-areas { display: flex; flex-direction: column; gap: 12px; margin-bottom: 32px; }
    .project-area { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; }
    .area-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; }
    .area-name { font-weight: 600; font-size: 15px; color: #0f172a; }
    .area-count { font-size: 12px; color: #64748b; background: #f1f5f9; padding: 2px 8px; border-radius: 4px; }
    .area-desc { font-size: 14px; color: #475569; line-height: 1.5; }
    .narrative { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 20px; margin-bottom: 24px; }
    .narrative p { margin-bottom: 12px; font-size: 14px; color: #475569; line-height: 1.7; }
    .key-insight { background: #f0fdf4; border: 1px solid #bbf7d0; border-radius: 8px; padding: 12px 16px; margin-top: 12px; font-size: 14px; color: #166534; }
    .section-intro { font-size: 14px; color: #64748b; margin-bottom: 16px; }
    .big-wins { display: flex; flex-direction: column; gap: 12px; margin-bottom: 24px; }
    .big-win { background: #f0fdf4; border: 1px solid #bbf7d0; border-radius: 8px; padding: 16px; }
    .big-win-title { font-weight: 600; font-size: 15px; color: #166534; margin-bottom: 8px; }
    .big-win-desc { font-size: 14px; color: #15803d; line-height: 1.5; }
    .friction-categories { display: flex; flex-direction: column; gap: 16px; margin-bottom: 24px; }
    .friction-category { background: #fef2f2; border: 1px solid #fca5a5; border-radius: 8px; padding: 16px; }
    .friction-title { font-weight: 600; font-size: 15px; color: #991b1b; margin-bottom: 6px; }
    .friction-desc { font-size: 13px; color: #7f1d1d; margin-bottom: 10px; }
    .friction-examples { margin: 0 0 0 20px; font-size: 13px; color: #334155; }
    .friction-examples li { margin-bottom: 4px; }
    .claude-md-section { background: #eff6ff; border: 1px solid #bfdbfe; border-radius: 8px; padding: 16px; margin-bottom: 20px; }
    .claude-md-section h3 { font-size: 14px; font-weight: 600; color: #1e40af; margin: 0 0 12px 0; }
    .claude-md-actions { margin-bottom: 12px; padding-bottom: 12px; border-bottom: 1px solid #dbeafe; }
    .copy-all-btn { background: #2563eb; color: white; border: none; border-radius: 4px; padding: 6px 12px; font-size: 12px; cursor: pointer; font-weight: 500; transition: all 0.2s; }
    .copy-all-btn:hover { background: #1d4ed8; }
    .copy-all-btn.copied { background: #16a34a; }
    .claude-md-item { display: flex; flex-wrap: wrap; align-items: flex-start; gap: 8px; padding: 10px 0; border-bottom: 1px solid #dbeafe; }
    .claude-md-item:last-child { border-bottom: none; }
    .cmd-checkbox { margin-top: 2px; }
    .cmd-code { background: white; padding: 8px 12px; border-radius: 4px; font-size: 12px; color: #1e40af; border: 1px solid #bfdbfe; font-family: monospace; display: block; white-space: pre-wrap; word-break: break-word; flex: 1; }
    .cmd-why { font-size: 12px; color: #64748b; width: 100%; padding-left: 24px; margin-top: 4px; }
    .features-section, .patterns-section { display: flex; flex-direction: column; gap: 12px; margin: 16px 0; }
    .feature-card { background: #f0fdf4; border: 1px solid #86efac; border-radius: 8px; padding: 16px; }
    .pattern-card { background: #f0f9ff; border: 1px solid #7dd3fc; border-radius: 8px; padding: 16px; }
    .feature-title, .pattern-title { font-weight: 600; font-size: 15px; color: #0f172a; margin-bottom: 6px; }
    .feature-oneliner { font-size: 14px; color: #475569; margin-bottom: 8px; }
    .pattern-summary { font-size: 14px; color: #475569; margin-bottom: 8px; }
    .feature-why, .pattern-detail { font-size: 13px; color: #334155; line-height: 1.5; }
    .feature-examples { margin-top: 12px; }
    .feature-example { padding: 8px 0; border-top: 1px solid #d1fae5; }
    .feature-example:first-child { border-top: none; }
    .example-desc { font-size: 13px; color: #334155; margin-bottom: 6px; }
    .example-code-row { display: flex; align-items: flex-start; gap: 8px; }
    .example-code { flex: 1; background: #f1f5f9; padding: 8px 12px; border-radius: 4px; font-family: monospace; font-size: 12px; color: #334155; overflow-x: auto; white-space: pre-wrap; }
    .copyable-prompt-section { margin-top: 12px; padding-top: 12px; border-top: 1px solid #e2e8f0; }
    .copyable-prompt-row { display: flex; align-items: flex-start; gap: 8px; }
    .copyable-prompt { flex: 1; background: #f8fafc; padding: 10px 12px; border-radius: 4px; font-family: monospace; font-size: 12px; color: #334155; border: 1px solid #e2e8f0; white-space: pre-wrap; line-height: 1.5; }
    .feature-code { background: #f8fafc; padding: 12px; border-radius: 6px; margin-top: 12px; border: 1px solid #e2e8f0; display: flex; align-items: flex-start; gap: 8px; }
    .feature-code code { flex: 1; font-family: monospace; font-size: 12px; color: #334155; white-space: pre-wrap; }
    .pattern-prompt { background: #f8fafc; padding: 12px; border-radius: 6px; margin-top: 12px; border: 1px solid #e2e8f0; }
    .pattern-prompt code { font-family: monospace; font-size: 12px; color: #334155; display: block; white-space: pre-wrap; margin-bottom: 8px; }
    .prompt-label { font-size: 11px; font-weight: 600; text-transform: uppercase; color: #64748b; margin-bottom: 6px; }
    .copy-btn { background: #e2e8f0; border: none; border-radius: 4px; padding: 4px 8px; font-size: 11px; cursor: pointer; color: #475569; flex-shrink: 0; }
    .copy-btn:hover { background: #cbd5e1; }
    .charts-row { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; margin: 24px 0; }
    .chart-card { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; }
    .chart-title { font-size: 12px; font-weight: 600; color: #64748b; text-transform: uppercase; margin-bottom: 12px; }
    .bar-row { display: flex; align-items: center; margin-bottom: 6px; }
    .bar-label { width: 100px; font-size: 11px; color: #475569; flex-shrink: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .bar-track { flex: 1; height: 6px; background: #f1f5f9; border-radius: 3px; margin: 0 8px; }
    .bar-fill { height: 100%; border-radius: 3px; }
    .bar-value { width: 28px; font-size: 11px; font-weight: 500; color: #64748b; text-align: right; }
    .empty { color: #94a3b8; font-size: 13px; }
    .horizon-section { display: flex; flex-direction: column; gap: 16px; }
    .horizon-card { background: linear-gradient(135deg, #faf5ff 0%, #f5f3ff 100%); border: 1px solid #c4b5fd; border-radius: 8px; padding: 16px; }
    .horizon-title { font-weight: 600; font-size: 15px; color: #5b21b6; margin-bottom: 8px; }
    .horizon-possible { font-size: 14px; color: #334155; margin-bottom: 10px; line-height: 1.5; }
    .horizon-tip { font-size: 13px; color: #6b21a8; background: rgba(255,255,255,0.6); padding: 8px 12px; border-radius: 4px; }
    .feedback-header { margin-top: 48px; color: #64748b; font-size: 16px; }
    .feedback-intro { font-size: 13px; color: #94a3b8; margin-bottom: 16px; }
    .feedback-section { margin-top: 16px; }
    .feedback-section h3 { font-size: 14px; font-weight: 600; color: #475569; margin-bottom: 12px; }
    .feedback-card { background: white; border: 1px solid #e2e8f0; border-radius: 8px; padding: 16px; margin-bottom: 12px; }
    .feedback-card.team-card { background: #eff6ff; border-color: #bfdbfe; }
    .feedback-card.model-card { background: #faf5ff; border-color: #e9d5ff; }
    .feedback-title { font-weight: 600; font-size: 14px; color: #0f172a; margin-bottom: 6px; }
    .feedback-detail { font-size: 13px; color: #475569; line-height: 1.5; }
    .feedback-evidence { font-size: 12px; color: #64748b; margin-top: 8px; }
    .fun-ending { background: linear-gradient(135deg, #fef3c7 0%, #fde68a 100%); border: 1px solid #fbbf24; border-radius: 12px; padding: 24px; margin-top: 40px; text-align: center; }
    .fun-headline { font-size: 18px; font-weight: 600; color: #78350f; margin-bottom: 8px; }
    .fun-detail { font-size: 14px; color: #92400e; }
    .collapsible-section { margin-top: 16px; }
    .collapsible-header { display: flex; align-items: center; gap: 8px; cursor: pointer; padding: 12px 0; border-bottom: 1px solid #e2e8f0; }
    .collapsible-header h3 { margin: 0; font-size: 14px; font-weight: 600; color: #475569; }
    .collapsible-arrow { font-size: 12px; color: #94a3b8; transition: transform 0.2s; }
    .collapsible-content { display: none; padding-top: 16px; }
    .collapsible-content.open { display: block; }
    .collapsible-header.open .collapsible-arrow { transform: rotate(90deg); }
    @media (max-width: 640px) { .charts-row { grid-template-columns: 1fr; } .stats-row { justify-content: center; } }
  </style>
</head>
<body>
  <div class="container">
    <h1>LingXi Insights</h1>
    <p class="subtitle">${e.total_messages.toLocaleString()} messages across ${e.total_sessions} sessions${e.total_sessions_scanned&&e.total_sessions_scanned>e.total_sessions?` (${e.total_sessions_scanned.toLocaleString()} total)`:""} | ${e.date_range.start} to ${e.date_range.end}</p>

    ${m}

    ${g}

    <nav class="nav-toc">
      <a href="#section-work">What You Work On</a>
      <a href="#section-usage">How You Use CC</a>
      <a href="#section-wins">Impressive Things</a>
      <a href="#section-friction">Where Things Go Wrong</a>
      <a href="#section-features">Features to Try</a>
      <a href="#section-patterns">New Usage Patterns</a>
      <a href="#section-horizon">On the Horizon</a>
      <a href="#section-feedback">Team Feedback</a>
    </nav>

    <div class="stats-row">
      <div class="stat"><div class="stat-value">${e.total_messages.toLocaleString()}</div><div class="stat-label">Messages</div></div>
      <div class="stat"><div class="stat-value">+${e.total_lines_added.toLocaleString()}/-${e.total_lines_removed.toLocaleString()}</div><div class="stat-label">Lines</div></div>
      <div class="stat"><div class="stat-value">${e.total_files_modified}</div><div class="stat-label">Files</div></div>
      <div class="stat"><div class="stat-value">${e.days_active}</div><div class="stat-label">Days</div></div>
      <div class="stat"><div class="stat-value">${e.messages_per_day}</div><div class="stat-label">Msgs/Day</div></div>
    </div>

    ${w}

    <div class="charts-row">
      <div class="chart-card">
        <div class="chart-title">What You Wanted</div>
        ${ee(e.goal_categories,"#2563eb")}
      </div>
      <div class="chart-card">
        <div class="chart-title">Top Tools Used</div>
        ${ee(e.tool_counts,"#0891b2")}
      </div>
    </div>

    <div class="charts-row">
      <div class="chart-card">
        <div class="chart-title">Languages</div>
        ${ee(e.languages,"#10b981")}
      </div>
      <div class="chart-card">
        <div class="chart-title">Session Types</div>
        ${ee(e.session_types||{},"#8b5cf6")}
      </div>
    </div>

    ${f}

    <!-- Response Time Distribution -->
    <div class="chart-card" style="margin: 24px 0;">
      <div class="chart-title">User Response Time Distribution</div>
      ${Dt(e.user_response_times)}
      <div style="font-size: 12px; color: #64748b; margin-top: 8px;">
        Median: ${e.median_response_time.toFixed(1)}s &bull; Average: ${e.avg_response_time.toFixed(1)}s
      </div>
    </div>

    <!-- Multi-clauding Section (matching Python reference) -->
    <div class="chart-card" style="margin: 24px 0;">
      <div class="chart-title">Multi-Clauding (Parallel Sessions)</div>
      ${e.multi_clauding.overlap_events===0?`
        <p style="font-size: 14px; color: #64748b; padding: 8px 0;">
          No parallel session usage detected. You typically work with one LingXi session at a time.
        </p>
      `:`
        <div style="display: flex; gap: 24px; margin: 12px 0;">
          <div style="text-align: center;">
            <div style="font-size: 24px; font-weight: 700; color: #7c3aed;">${e.multi_clauding.overlap_events}</div>
            <div style="font-size: 11px; color: #64748b; text-transform: uppercase;">Overlap Events</div>
          </div>
          <div style="text-align: center;">
            <div style="font-size: 24px; font-weight: 700; color: #7c3aed;">${e.multi_clauding.sessions_involved}</div>
            <div style="font-size: 11px; color: #64748b; text-transform: uppercase;">Sessions Involved</div>
          </div>
          <div style="text-align: center;">
            <div style="font-size: 24px; font-weight: 700; color: #7c3aed;">${e.total_messages>0?Math.round(100*e.multi_clauding.user_messages_during/e.total_messages):0}%</div>
            <div style="font-size: 11px; color: #64748b; text-transform: uppercase;">Of Messages</div>
          </div>
        </div>
        <p style="font-size: 13px; color: #475569; margin-top: 12px;">
          You run multiple LingXi sessions simultaneously. Parallel usage is detected when sessions
          overlap in time, suggesting parallel workflows.
        </p>
      `}
    </div>

    <!-- Time of Day & Tool Errors -->
    <div class="charts-row">
      <div class="chart-card">
        <div class="chart-title" style="display: flex; align-items: center; gap: 12px;">
          User Messages by Time of Day
          <select id="timezone-select" style="font-size: 12px; padding: 4px 8px; border-radius: 4px; border: 1px solid #e2e8f0;">
            <option value="0">PT (UTC-8)</option>
            <option value="3">ET (UTC-5)</option>
            <option value="8">London (UTC)</option>
            <option value="9">CET (UTC+1)</option>
            <option value="17">Tokyo (UTC+9)</option>
            <option value="custom">Custom offset...</option>
          </select>
          <input type="number" id="custom-offset" placeholder="UTC offset" style="display: none; width: 80px; font-size: 12px; padding: 4px; border-radius: 4px; border: 1px solid #e2e8f0;">
        </div>
        ${Nt(e.message_hours)}
      </div>
      <div class="chart-card">
        <div class="chart-title">Tool Errors Encountered</div>
        ${Object.keys(e.tool_error_categories).length>0?ee(e.tool_error_categories,"#dc2626"):'<p class="empty">No tool errors</p>'}
      </div>
    </div>

    ${T}

    <div class="charts-row">
      <div class="chart-card">
        <div class="chart-title">What Helped Most (LingXi's Capabilities)</div>
        ${ee(e.success,"#16a34a")}
      </div>
      <div class="chart-card">
        <div class="chart-title">Outcomes</div>
        ${ee(e.outcomes,"#8b5cf6",6,jt)}
      </div>
    </div>

    ${I}

    <div class="charts-row">
      <div class="chart-card">
        <div class="chart-title">Primary Friction Types</div>
        ${ee(e.friction,"#dc2626")}
      </div>
      <div class="chart-card">
        <div class="chart-title">Inferred Satisfaction (model-estimated)</div>
        ${ee(e.satisfaction,"#eab308",6,Pt)}
      </div>
    </div>

    ${K}

    ${W}

    ${ie}

    ${oe}
  </div>
  <script>${D}</script>
</body>
</html>`}
export function render_report_html(data, insights, autoModeRecommendation = null) {
  return Ft(data, insights, autoModeRecommendation);
}
