/* Served by the run-scoped LingXi loopback gateway. No native message bridge. */
(() => {
  if (window.top !== window || window.localApp) return;
  const config = __LINGXI_APP_CONFIGURATION__;
  const base = `${location.origin}/__run/${encodeURIComponent(config.runId)}/`;
  const invoke = async (method, args = {}) => {
    const body = JSON.stringify({method, args});
    if (body.length > 24 * 1024 * 1024) throw new Error('Application request is too large');
    let response;
    try {
      response = await fetch(`${base}__host/agent`, {
        method: 'POST', credentials: 'omit', cache: 'no-store', redirect: 'error',
        headers: {'Content-Type': 'application/json', 'X-LingXi-App-Request': '1'}, body,
        signal: AbortSignal.timeout(30000)
      });
    } catch (cause) {
      const error = new Error('LingXi is unavailable or paused. Return to LingXi to resume this application.', {cause});
      error.code = 'host_unavailable'; throw error;
    }
    if (!response.ok) { const error = new Error('The application run is unavailable. Return to LingXi to resume.'); error.code = 'host_unavailable'; throw error; }
    const result = await response.json();
    if (!result.ok) { const error = new Error(result.error?.message || 'Agent request failed'); error.code = result.error?.code; throw error; }
    return result.value;
  };
  const subscribe = (runId, listener) => {
    if (typeof listener !== 'function') throw new TypeError('A listener is required');
    let active = true, timer, sequence = 0;
    const poll = async () => {
      try {
        const run = await invoke('agent.get', {runId});
        if (!active) return;
        if (run.sequence > sequence) { sequence = run.sequence; listener({type:'state', runId, sequence, time:run.updatedAt, run}); }
        if (['succeeded','failed','cancelled','interrupted'].includes(run.state)) active = false;
      } catch (error) { if (active) listener({type:'connection', runId, error:{code:error.code || 'host_unavailable', message:error.message}}); }
      if (active) timer = setTimeout(poll, 1000);
    };
    poll(); return () => { active = false; clearTimeout(timer); };
  };
  // The browser, not LingXi's native picker, owns the file/camera interaction.
  // Normalize on a bounded canvas, including EXIF orientation via browser decode.
  const photo = capture => new Promise((resolve, reject) => {
    const input = document.createElement('input'); input.type = 'file'; input.accept = 'image/*';
    if (capture) input.setAttribute('capture', 'environment');
    input.style.display = 'none'; document.body.append(input);
    const cleanup = () => input.remove();
    input.addEventListener('cancel', () => { cleanup(); resolve(null); }, {once:true});
    input.addEventListener('change', async () => {
      let image, url;
      try {
        const file = input.files?.[0]; if (!file) { resolve(null); return; }
        if (file.size > 16 * 1024 * 1024) throw new Error('Choose an image smaller than 16 MB');
        url = URL.createObjectURL(file); image = new Image(); image.src = url; await image.decode();
        const {naturalWidth:w, naturalHeight:h} = image;
        if (!w || !h || w * h > 64000000 || Math.max(w,h) > 32768) throw new Error('Image dimensions are too large');
        const scale = Math.min(1, 2048 / Math.max(w,h)); const canvas = document.createElement('canvas');
        canvas.width = Math.max(1, Math.round(w*scale)); canvas.height = Math.max(1, Math.round(h*scale));
        const context = canvas.getContext('2d'); if (!context) throw new Error('Image decoding is unavailable');
        context.fillStyle = '#fff'; context.fillRect(0,0,canvas.width,canvas.height); context.drawImage(image,0,0,canvas.width,canvas.height);
        const base64 = canvas.toDataURL('image/jpeg', 0.85).split(',')[1];
        if (!base64 || base64.length > 12 * 1024 * 1024) throw new Error('The normalized image is too large');
        resolve({mimeType:'image/jpeg', base64});
      } catch (error) { reject(error); }
      finally { if (url) URL.revokeObjectURL(url); cleanup(); }
    }, {once:true});
    input.click();
  });
  const serviceUrl = (name, path = '/') => {
    if (!config.services.includes(name)) throw new Error('Unknown application service');
    if (typeof path !== 'string' || !path.startsWith('/') || path.startsWith('//') || path.includes('#') || path.includes('\\')) throw new Error('Invalid service path');
    let decoded; try { decoded = decodeURIComponent(path.split('?')[0]); } catch { throw new Error('Invalid service path'); }
    if (/%2f|%5c/i.test(path.split('?')[0]) || decoded.includes('\\') || decoded.includes('\0') || decoded.split('/').some(p=>p==='.'||p==='..')) throw new Error('Invalid service path');
    return `${base}__service/${encodeURIComponent(name)}${path}`;
  };
  Object.defineProperty(window, 'localApp', {configurable:false, writable:false, value:Object.freeze({
    info:async () => ({id:config.appId, name:config.name, platform:'mobile-browser', hostPlatform:config.hostPlatform, revision:config.revision, runId:config.runId}),
    agent:Object.freeze({run:input=>invoke('agent.run',input), get:runId=>invoke('agent.get',{runId}), cancel:runId=>invoke('agent.cancel',{runId}), subscribe}),
    host:Object.freeze({open:()=>{ location.href = `lingxi://apps/${encodeURIComponent(config.appId)}?review=agent`; }}),
    camera:Object.freeze({capture:()=>photo(true)}), photos:Object.freeze({pick:()=>photo(false)}),
    serviceUrl, serviceSocketUrl:async (name,path='/')=>serviceUrl(name,path).replace(/^http:/,'ws:')
  })});
})();
