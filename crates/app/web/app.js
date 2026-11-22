'use strict';

const grid = document.querySelector('#cameras');
const message = document.querySelector('#message');
const refresh = document.querySelector('#refresh');
let tiles = [];
let refreshGeneration = 0;

async function request(url, options = {}) {
  const response = await fetch(url, { credentials: 'same-origin', ...options });
  if (!response.ok) {
    const contentType = response.headers.get('content-type') || '';
    const detail = contentType.includes('application/json') ? (await response.json().catch(() => ({}))).error : '';
    throw new Error(`${response.status} ${response.statusText}${detail ? `: ${detail}` : ''}`);
  }
  if (response.status === 204) return null;
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}

function jsonPost(url, body, keepalive = false) {
  return request(url, {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body), keepalive,
  });
}

function deleteSession(id, keepalive = false) {
  return request(`/api/session/${encodeURIComponent(id)}`, { method: 'DELETE', keepalive });
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(text, label) {
  const node = element('button', '', text);
  node.type = 'button';
  if (label) node.setAttribute('aria-label', label);
  return node;
}

function gathered(pc) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve, reject) => {
    const changed = () => {
      if (pc.iceGatheringState === 'complete') { cleanup(); resolve(); }
      else if (pc.signalingState === 'closed') { cleanup(); reject(new Error('Connection closed')); }
    };
    const cleanup = () => {
      pc.removeEventListener('icegatheringstatechange', changed);
      pc.removeEventListener('signalingstatechange', changed);
    };
    pc.addEventListener('icegatheringstatechange', changed);
    pc.addEventListener('signalingstatechange', changed);
    changed();
  });
}

function cameraTile(camera) {
  const card = element('article', 'camera');
  card.tabIndex = 0;
  const head = element('div', 'camera-head');
  head.append(element('h2', '', camera.name));
  const expand = button('Expand');
  head.append(expand);
  const video = element('video');
  video.autoplay = true;
  video.playsInline = true;
  video.muted = true;
  const controls = element('div', 'controls');
  const profiles = element('select');
  profiles.setAttribute('aria-label', `Profile for ${camera.name}`);
  for (const profile of camera.profiles) {
    const option = element('option', '', profile.name);
    option.value = profile.token;
    profiles.append(option);
  }
  const play = button('Play');
  controls.append(profiles, play);
  const audio = element('div', 'audio');
  const mute = button('Unmute');
  const volumeLabel = element('label', '', 'Volume');
  const volume = element('input');
  volume.type = 'range'; volume.min = '0'; volume.max = '1'; volume.step = '0.05'; volume.value = '1';
  volumeLabel.append(volume); audio.append(mute, volumeLabel);
  const status = element('p', 'status', camera.profiles.length ? 'Stopped' : 'No profiles available');
  status.setAttribute('role', 'status');
  const ptz = element('div', 'ptz');
  ptz.setAttribute('aria-label', 'Pan and tilt controls');
  const directions = [
    ['up', '↑', 'Tilt up', 0, 1], ['left', '←', 'Pan left', -1, 0],
    ['down', '↓', 'Tilt down', 0, -1], ['right', '→', 'Pan right', 1, 0],
  ];
  const directionButtons = [];
  let current = null;
  let generation = 0;
  let held = null;
  let disposed = false;

  function setStatus(text, error = false) {
    status.textContent = text;
    status.classList.toggle('error', error);
  }
  function selectedProfile() { return camera.profiles.find(profile => profile.token === profiles.value); }
  function update() {
    const active = current !== null;
    play.textContent = active ? 'Stop' : 'Play';
    play.disabled = !camera.profiles.length;
    const available = Boolean(current && current.id && selectedProfile()?.ptz);
    for (const control of directionButtons) control.disabled = !available;
    ptz.hidden = !selectedProfile()?.ptz;
    help.hidden = ptz.hidden;
  }
  function queuePtz(session, x, y, keepalive = false) {
    if (!session.id) return;
    session.ptzPending++;
    session.ptzQueue = session.ptzQueue.then(() => jsonPost('/api/ptz', {
      camera_id: camera.id, profile_token: session.token, session_id: session.id, x, y,
    }, keepalive)).catch(error => {
      if (!disposed) setStatus(`PTZ: ${error.message}`, true);
    }).finally(() => { session.ptzPending--; });
  }
  function release(keepalive = false) {
    if (!held) return;
    clearInterval(held.timer);
    const session = held.session;
    held = null;
    queuePtz(session, 0, 0, keepalive);
  }
  function move(x, y, source) {
    if (!current?.id || !selectedProfile()?.ptz) return;
    if (held && held.source === source) return;
    release();
    const session = current;
    held = { session, source, timer: setInterval(() => {
      if (!session.ptzPending) queuePtz(session, x, y);
    }, 500) };
    queuePtz(session, x, y);
  }
  for (const [name, symbol, label, x, y] of directions) {
    const control = button(symbol, label);
    control.className = name;
    control.addEventListener('pointerdown', event => {
      if (event.button !== 0) return;
      event.preventDefault();
      control.setPointerCapture(event.pointerId);
      move(x, y, `pointer:${event.pointerId}`);
    });
    for (const eventName of ['pointerup', 'pointercancel', 'lostpointercapture']) {
      control.addEventListener(eventName, event => {
        if (held?.source === `pointer:${event.pointerId}`) release();
      });
    }
    directionButtons.push(control); ptz.append(control);
  }
  const help = element('p', 'ptz-help', 'Hold a direction or use arrow keys while this camera is focused.');
  const keys = { ArrowUp: [0, 1], ArrowDown: [0, -1], ArrowLeft: [-1, 0], ArrowRight: [1, 0] };
  function keydown(event) {
    if (!keys[event.key] || event.target.closest('select, input')) return;
    if (!current?.id || !selectedProfile()?.ptz) return;
    event.preventDefault();
    move(...keys[event.key], `key:${event.key}`);
  }
  function keyup(event) {
    if (held?.source === `key:${event.key}`) release();
  }
  function blur() { release(true); }
  card.addEventListener('keydown', keydown);
  card.addEventListener('focusout', event => {
    if (!card.contains(event.relatedTarget)) release();
  });
  window.addEventListener('keyup', keyup);
  window.addEventListener('blur', blur);

  function stop(keepalive = false) {
    ++generation;
    const session = current;
    release(keepalive);
    current = null;
    video.srcObject = null;
    if (session) {
      clearInterval(session.heartbeat);
      session.pc.close();
      if (session.id) {
        session.ptzQueue.then(() => deleteSession(session.id, keepalive)).catch(error => {
          if (!disposed) setStatus(`Stopping session: ${error.message}`, true);
        });
      }
    }
    if (!disposed) { setStatus('Stopped'); update(); }
  }
  async function start() {
    const profile = selectedProfile();
    if (!profile) return;
    const myGeneration = ++generation;
    let pc;
    let session;
    try {
      pc = new RTCPeerConnection();
      session = { pc, token: profile.token, id: null, ptzQueue: Promise.resolve(), ptzPending: 0, heartbeat: null };
      current = session;
      update(); setStatus('Connecting…');
      pc.addTransceiver('video', { direction: 'recvonly' });
      pc.addTransceiver('audio', { direction: 'recvonly' });
      const stream = new MediaStream();
      video.srcObject = stream;
      pc.addEventListener('track', event => {
        if (current !== session) return;
        stream.addTrack(event.track);
        video.play().catch(error => { if (current === session) setStatus(`Playback: ${error.message}`, true); });
      });
      pc.addEventListener('connectionstatechange', () => {
        if (current !== session) return;
        if (pc.connectionState === 'connected') setStatus('Playing');
        if (['failed', 'disconnected', 'closed'].includes(pc.connectionState)) {
          const state = pc.connectionState;
          stop(); setStatus(`Connection ${state}`, true);
        }
      });
      await pc.setLocalDescription(await pc.createOffer());
      await gathered(pc);
      if (generation !== myGeneration) return;
      const result = await jsonPost('/api/session', {
        camera_id: camera.id, profile_token: profile.token, offer: pc.localDescription.sdp,
      });
      if (!result || !result.id || !result.answer) throw new Error('Invalid session response');
      if (generation !== myGeneration) {
        await deleteSession(result.id, true);
        return;
      }
      session.id = result.id;
      session.heartbeat = setInterval(() => {
        jsonPost(`/api/session/${encodeURIComponent(session.id)}/heartbeat`, {}).catch(error => {
          if (current === session) setStatus(`Session heartbeat: ${error.message}`, true);
        });
      }, 10000);
      update();
      await pc.setRemoteDescription({ type: 'answer', sdp: result.answer });
    } catch (error) {
      if (generation === myGeneration) {
        stop(); setStatus(`Unable to play: ${error.message}`, true);
      } else if (pc && pc.signalingState !== 'closed') pc.close();
    }
  }
  play.addEventListener('click', () => { if (current) stop(); else start(); });
  profiles.addEventListener('change', () => {
    const wasPlaying = Boolean(current);
    stop();
    if (wasPlaying) start();
  });
  expand.addEventListener('click', () => {
    const expanded = card.classList.toggle('expanded');
    expand.textContent = expanded ? 'Collapse' : 'Expand';
    expand.setAttribute('aria-expanded', String(expanded));
  });
  mute.addEventListener('click', () => {
    video.muted = !video.muted;
    mute.textContent = video.muted ? 'Unmute' : 'Mute';
  });
  volume.addEventListener('input', () => { video.volume = Number(volume.value); });
  card.append(head, video, controls, audio, ptz, help, status);
  update();
  return {
    card,
    dispose(keepalive = false) {
      disposed = true;
      stop(keepalive);
      window.removeEventListener('keyup', keyup);
      window.removeEventListener('blur', blur);
    },
  };
}

async function loadCameras() {
  const generation = ++refreshGeneration;
  refresh.disabled = true;
  message.className = '';
  message.textContent = 'Loading cameras…';
  try {
    const cameras = await request('/api/cameras');
    if (generation !== refreshGeneration) return;
    if (!Array.isArray(cameras)) throw new Error('Invalid camera list');
    const newTiles = cameras.map(cameraTile);
    for (const tile of tiles) tile.dispose();
    tiles = newTiles;
    grid.replaceChildren(...tiles.map(tile => tile.card));
    message.textContent = tiles.length ? '' : 'No cameras available.';
  } catch (error) {
    if (generation !== refreshGeneration) return;
    message.className = 'error';
    message.textContent = `Unable to load cameras: ${error.message}`;
  } finally {
    if (generation === refreshGeneration) refresh.disabled = false;
  }
}
refresh.addEventListener('click', loadCameras);
window.addEventListener('pagehide', () => {
  for (const tile of tiles) tile.dispose(true);
  tiles = [];
});
loadCameras();
