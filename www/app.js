'use strict';

// The channel page. Opens with a handshake (POST /api/visit) that names this
// browser's device, draws posts from GET /api/posts, loads older ones as the
// reader scrolls up, and reports a post to POST /api/views once it has been
// at least half on screen for two continuous seconds — at most once per post
// per page load, so fast scrolling counts nothing and coming back counts again.
// A message opens a menu (Copy Text, Copy Link, Forward) on right click or long
// press, and the header opens the channel info.

(() => {
  const DEVICE_KEY = 'plogia.device';
  const DWELL_MS = 2000;
  const MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July',
    'August', 'September', 'October', 'November', 'December'];
  const FORWARD = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M14 5v4C7 10 4 15 3 20c2.5-3.5 6-5.1 11-5.1V19l7-7-7-7z"/></svg>';
  const EYE = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 5C6.6 5 2.9 9.2 1.6 11.1a1.6 1.6 0 0 0 0 1.8C2.9 14.8 6.6 19 12 19s9.1-4.2 10.4-6.1a1.6 1.6 0 0 0 0-1.8C21.1 9.2 17.4 5 12 5zm0 11.5a4.5 4.5 0 1 1 0-9 4.5 4.5 0 0 1 0 9zm0-7a2.5 2.5 0 1 0 0 5 2.5 2.5 0 0 0 0-5z"/></svg>';

  const feed = document.getElementById('feed');
  const list = document.getElementById('posts');
  const olderSentinel = document.getElementById('older');
  const newerSentinel = document.getElementById('newer');
  const down = document.getElementById('down');
  const subscribers = document.getElementById('subscribers');
  const lightbox = document.getElementById('lightbox');
  const menu = document.getElementById('menu');
  const toast = document.getElementById('toast');
  const info = document.getElementById('info');
  const viewer = document.getElementById('viewer');
  const channel = feed.dataset.channel;
  const avatar = feed.dataset.avatar;
  const focus = feed.dataset.post ? Number(feed.dataset.post) : null;

  const state = { older: false, newer: false, loading: false, first: null, last: null };

  // ---- Handshake ----------------------------------------------------------

  // Automated browsers announce themselves here; they get no device, so they
  // never send a view.
  const handshake = navigator.webdriver === true ? Promise.resolve(null) : (async () => {
    let stored = null;
    try { stored = localStorage.getItem(DEVICE_KEY); } catch (_) { /* storage blocked */ }
    try {
      const response = await fetch('/api/visit', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ device: stored, path: location.pathname, referrer: document.referrer }),
      });
      // 204: the server took this for a robot.
      if (response.status !== 200) return null;
      const body = await response.json();
      try { localStorage.setItem(DEVICE_KEY, body.device); } catch (_) { /* storage blocked */ }
      const count = body.subscribers;
      subscribers.textContent = `${count.toLocaleString('en')} subscriber${count === 1 ? '' : 's'}`;
      document.getElementById('info-subscribers').textContent = subscribers.textContent;
      return body.device;
    } catch (error) {
      console.error(error);
      return null;
    }
  })();

  // ---- Views --------------------------------------------------------------

  const counted = new Set();  // posts already sent during this page load
  const onScreen = new Set(); // posts at least half visible right now
  const timers = new Map();   // post -> its pending two-second timer
  let queue = [];
  let flushTimer = null;

  function arm(id) {
    if (counted.has(id) || timers.has(id) || document.visibilityState !== 'visible') return;
    timers.set(id, setTimeout(() => {
      timers.delete(id);
      counted.add(id);
      queue.push(id);
      flush(false);
    }, DWELL_MS));
  }

  function disarm(id) {
    clearTimeout(timers.get(id));
    timers.delete(id);
  }

  // Fine-grained thresholds so a post several screens tall still gets a
  // callback once enough of it is showing.
  const thresholds = [];
  for (let step = 0; step <= 50; step++) thresholds.push(step / 50);

  const watcher = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      const id = Number(entry.target.dataset.id);
      const seen = entry.isIntersecting && (entry.intersectionRatio >= 0.5
        || entry.intersectionRect.height >= window.innerHeight * 0.5);
      if (seen) {
        onScreen.add(id);
        arm(id);
      } else {
        onScreen.delete(id);
        disarm(id);
      }
    }
  }, { threshold: thresholds });

  // A post behind another tab is not being read.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      for (const id of onScreen) arm(id);
    } else {
      for (const id of [...timers.keys()]) disarm(id);
      flush(true);
    }
  });
  addEventListener('pagehide', () => flush(true));

  // Views made in the same moment go out as one request. `now` sends at once,
  // for a page being hidden or closed, where a timer would never fire.
  function flush(now) {
    if (now) {
      clearTimeout(flushTimer);
      flushTimer = null;
      send();
    } else if (flushTimer === null) {
      flushTimer = setTimeout(() => {
        flushTimer = null;
        send();
      }, 300);
    }
  }

  let device = null;
  handshake.then((value) => { device = value; });

  async function send() {
    if (queue.length === 0) return;
    const posts = queue;
    queue = [];
    // Already settled on any page that has been open for two seconds, so on
    // pagehide the request below still starts synchronously.
    const id = device !== null ? device : await handshake;
    if (!id) return;
    try {
      const response = await fetch('/api/views', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ device: id, posts }),
        keepalive: true,
      });
      if (response.status === 404) {
        // The database no longer knows this device; the next load gets a new one.
        try { localStorage.removeItem(DEVICE_KEY); } catch (_) { /* storage blocked */ }
        return;
      }
      if (response.status !== 200) return;
      const body = await response.json();
      for (const [post, count] of Object.entries(body.views)) {
        const number = document.querySelector(`#post-${post} .views span`);
        if (number) number.textContent = shortCount(count);
      }
    } catch (error) {
      console.error(error);
    }
  }

  // ---- Drawing ------------------------------------------------------------

  // Telegram's style: 999, 1.2K, 15.3K, 152K, 1.2M.
  function shortCount(count) {
    const trim = (value) => String(value < 100 ? Math.floor(value * 10) / 10 : Math.floor(value));
    if (count < 1000) return String(count);
    if (count < 1e6) return trim(count / 1e3) + 'K';
    return trim(count / 1e6) + 'M';
  }

  function dayLabel(day) {
    const [year, month, date] = day.split('-').map(Number);
    const label = `${MONTHS[month - 1]} ${date}`;
    return year === new Date().getFullYear() ? label : `${label}, ${year}`;
  }

  function render(post) {
    const article = document.createElement('article');
    article.className = 'post';
    article.id = `post-${post.id}`;
    article.dataset.id = post.id;
    article.dataset.day = post.date.slice(0, 10);

    const picture = document.createElement('img');
    picture.className = 'avatar';
    picture.src = avatar;
    picture.alt = '';

    const bubble = document.createElement('div');
    bubble.className = 'bubble';

    const author = document.createElement('div');
    author.className = 'author';
    author.dir = 'auto';
    author.textContent = channel;

    const text = document.createElement('div');
    text.className = 'text';
    text.innerHTML = post.html;
    // Each block finds its own direction, so a Persian line and an English
    // line in one post each read the right way.
    for (const block of text.querySelectorAll('p, ul, ol, li, h1, h2, h3, h4, h5, h6, blockquote, th, td')) {
      block.dir = 'auto';
    }
    for (const link of text.querySelectorAll('a[href]')) {
      if (link.origin !== location.origin) {
        link.target = '_blank';
        link.rel = 'noopener noreferrer';
      }
    }

    const meta = document.createElement('div');
    meta.className = 'meta';
    meta.innerHTML = `<span class="views">${EYE}<span></span></span><a></a>`;
    meta.querySelector('.views span').textContent = shortCount(post.views);
    const time = meta.querySelector('a');
    time.href = `/p/${post.id}`;
    time.textContent = post.date.slice(11, 16);
    time.title = `${post.date.slice(0, 10)} ${post.date.slice(11, 16)}`;

    const forward = document.createElement('button');
    forward.className = 'forward';
    forward.type = 'button';
    forward.ariaLabel = 'Forward';
    forward.innerHTML = FORWARD;

    bubble.append(author, text, meta);
    article.append(picture, bubble, forward);
    return article;
  }

  // One sticky date pill in front of the first post of each day.
  function placeDays() {
    for (const pill of list.querySelectorAll('.day')) pill.remove();
    let previous = null;
    for (const article of list.querySelectorAll('.post')) {
      const day = article.dataset.day;
      if (day !== previous) {
        const pill = document.createElement('div');
        pill.className = 'day';
        pill.innerHTML = '<span></span>';
        pill.firstChild.textContent = dayLabel(day);
        article.before(pill);
        previous = day;
      }
    }
  }

  function add(posts, atTop) {
    const nodes = [];
    for (const post of posts) nodes.push(render(post));
    if (atTop) list.prepend(...nodes); else list.append(...nodes);
    for (const node of nodes) watcher.observe(node);
    placeDays();
  }

  function message(text) {
    const pill = document.createElement('p');
    pill.className = 'pill';
    pill.textContent = text;
    list.append(pill);
  }

  async function fetchPosts(query) {
    const response = await fetch(`/api/posts${query}`);
    if (!response.ok) throw new Error(`GET /api/posts${query}: ${response.status}`);
    const page = await response.json();
    document.getElementById('info-posts').textContent = `${page.total.toLocaleString('en')} post${page.total === 1 ? '' : 's'}`;
    return page;
  }

  // ---- Scrolling ----------------------------------------------------------

  // Until the reader touches anything, late-loading images must not push the
  // newest post (or the linked one) out of view.
  let pin = null;
  for (const event of ['wheel', 'touchstart', 'keydown', 'mousedown']) {
    addEventListener(event, () => { pin = null; }, { passive: true });
  }
  list.addEventListener('load', () => { if (pin) pin(); }, true);

  function near(element) {
    const box = element.getBoundingClientRect();
    return box.bottom > -800 && box.top < innerHeight + 800;
  }

  async function load(older) {
    if (state.loading || !(older ? state.older : state.newer)) return;
    state.loading = true;
    try {
      // Keep the post at the top of the screen where it is while older posts
      // arrive above it.
      const anchor = list.querySelector('.post');
      const before = anchor ? anchor.getBoundingClientRect().top : 0;
      const page = await fetchPosts(older ? `?before=${state.first}` : `?after=${state.last}`);
      if (page.posts.length === 0) {
        if (older) state.older = false; else state.newer = false;
      } else if (older) {
        add(page.posts, true);
        state.first = page.posts[0].id;
        state.older = page.older;
      } else {
        add(page.posts, false);
        state.last = page.posts[page.posts.length - 1].id;
        state.newer = page.newer;
      }
      if (older && anchor) scrollBy(0, anchor.getBoundingClientRect().top - before);
    } catch (error) {
      console.error(error);
      return;
    } finally {
      state.loading = false;
      updateDown();
    }
    fill();
  }

  // Keep loading while a sentinel is still close to the screen.
  function fill() {
    if (state.older && near(olderSentinel)) load(true);
    else if (state.newer && near(newerSentinel)) load(false);
  }

  function updateDown() {
    const fromBottom = document.documentElement.scrollHeight - scrollY - innerHeight;
    down.hidden = !(state.newer || fromBottom > 400);
  }

  down.addEventListener('click', () => {
    if (state.newer) {
      location.href = '/';
    } else {
      scrollTo({ top: document.documentElement.scrollHeight, behavior: 'smooth' });
    }
  });
  addEventListener('scroll', updateDown, { passive: true });

  // ---- Copy and forward ---------------------------------------------------

  let toastTimer = null;
  function notify(text) {
    toast.textContent = text;
    toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { toast.hidden = true; }, 2000);
  }

  // navigator.clipboard exists only in a secure context; plain HTTP on a LAN
  // address takes the old way.
  async function copy(text, done) {
    try {
      await navigator.clipboard.writeText(text);
    } catch (_) {
      const area = document.createElement('textarea');
      area.value = text;
      area.style.position = 'fixed';
      area.style.opacity = '0';
      document.body.append(area);
      area.select();
      document.execCommand('copy');
      area.remove();
    }
    notify(done);
  }

  // The system share sheet where there is one; elsewhere (desktop Linux, for
  // one) the link is copied instead.
  async function forward(url) {
    if (navigator.share) {
      try { await navigator.share({ url }); } catch (_) { /* dismissed */ }
      return;
    }
    copy(url, 'Link copied to clipboard');
  }

  // ---- Message menu -------------------------------------------------------

  let menuPost = null;

  function openMenu(article, x, y) {
    menuPost = article;
    menu.hidden = false;
    const box = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(8, Math.min(x, innerWidth - box.width - 8))}px`;
    menu.style.top = `${Math.max(8, Math.min(y, innerHeight - box.height - 8))}px`;
  }

  function closeMenu() {
    menu.hidden = true;
    menuPost = null;
  }

  list.addEventListener('contextmenu', (event) => {
    const bubble = event.target.closest('.bubble');
    if (!bubble) return;
    event.preventDefault();
    openMenu(bubble.closest('.post'), event.clientX, event.clientY);
  });

  // iOS never fires contextmenu, so a long press is timed by hand. The click
  // that would follow the press is swallowed, or it would follow a link.
  let press = null;
  let pressed = false;
  list.addEventListener('touchstart', (event) => {
    pressed = false;
    const bubble = event.target.closest('.bubble');
    if (!bubble || event.touches.length !== 1) return;
    const touch = event.touches[0];
    press = setTimeout(() => {
      pressed = true;
      openMenu(bubble.closest('.post'), touch.clientX, touch.clientY);
    }, 500);
  }, { passive: true });
  list.addEventListener('touchmove', () => clearTimeout(press), { passive: true });
  list.addEventListener('touchcancel', () => clearTimeout(press), { passive: true });
  list.addEventListener('touchend', (event) => {
    clearTimeout(press);
    if (pressed) event.preventDefault();
  });

  document.addEventListener('pointerdown', (event) => {
    if (!menu.hidden && !menu.contains(event.target)) closeMenu();
  });
  addEventListener('scroll', () => { if (!menu.hidden) closeMenu(); }, { passive: true });
  addEventListener('resize', () => { if (!menu.hidden) closeMenu(); });

  menu.addEventListener('click', (event) => {
    const item = event.target.closest('button');
    if (!item || !menuPost) return;
    const article = menuPost;
    closeMenu();
    const url = `${location.origin}/p/${article.dataset.id}`;
    if (item.dataset.action === 'text') {
      copy(article.querySelector('.text').innerText.trim(), 'Text copied to clipboard');
    } else if (item.dataset.action === 'link') {
      copy(url, 'Link copied to clipboard');
    } else {
      forward(url);
    }
  });

  // ---- Channel info -------------------------------------------------------

  // The panel, and the photo opened from it, are each a history entry, so the
  // back button closes the top one as on a phone.
  function openInfo() {
    if (!info.hidden) return;
    info.hidden = false;
    document.body.classList.add('info-open');
    history.pushState({ info: true }, '');
  }

  addEventListener('popstate', () => {
    const state = history.state || {};
    viewer.hidden = !state.photo;
    info.hidden = !state.info;
    document.body.classList.toggle('info-open', !info.hidden);
  });

  document.getElementById('info-photo').addEventListener('click', () => {
    viewer.hidden = false;
    history.pushState({ info: true, photo: true }, '');
  });
  document.getElementById('viewer-close').addEventListener('click', () => history.back());
  viewer.addEventListener('click', (event) => {
    if (event.target.closest('.viewer-stage') && event.target.tagName !== 'IMG') history.back();
  });

  document.getElementById('channel').addEventListener('click', openInfo);
  document.getElementById('info-close').addEventListener('click', () => history.back());
  document.getElementById('info-share').addEventListener('click', () => forward(`${location.origin}/`));
  document.getElementById('info-host').textContent = location.host;
  document.getElementById('info-link').addEventListener('click', () => {
    copy(`${location.origin}/`, 'Link copied to clipboard');
  });

  // ---- Clicks in the feed -------------------------------------------------

  list.addEventListener('click', (event) => {
    const button = event.target.closest('.forward');
    if (button) {
      forward(`${location.origin}/p/${button.closest('.post').dataset.id}`);
      return;
    }
    if (event.target.closest('.post > .avatar')) {
      openInfo();
      return;
    }
    const image = event.target.closest('.text img');
    if (!image) return;
    lightbox.firstElementChild.src = image.currentSrc || image.src;
    lightbox.hidden = false;
  });
  lightbox.addEventListener('click', () => { lightbox.hidden = true; });

  // Escape closes the top-most thing open.
  addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    if (!menu.hidden) closeMenu();
    else if (!lightbox.hidden) lightbox.hidden = true;
    else if (!viewer.hidden || !info.hidden) history.back();
  });

  // ---- Start --------------------------------------------------------------

  (async () => {
    let page;
    try {
      page = await fetchPosts(focus ? `?around=${focus}` : '');
    } catch (error) {
      console.error(error);
      message('Could not load the posts. Try again in a moment.');
      return;
    }
    state.older = page.older;
    state.newer = page.newer;
    if (page.posts.length > 0) {
      state.first = page.posts[0].id;
      state.last = page.posts[page.posts.length - 1].id;
    }
    add(page.posts, false);
    if (page.posts.length === 0) message('No posts yet.');

    const target = focus === null ? null : document.getElementById(`post-${focus}`);
    if (target) {
      target.classList.add('highlight');
      pin = () => target.scrollIntoView({ block: 'center' });
    } else {
      pin = () => scrollTo(0, document.documentElement.scrollHeight);
    }
    pin();
    updateDown();

    const sentinels = new IntersectionObserver(fill, { rootMargin: '800px 0px' });
    sentinels.observe(olderSentinel);
    sentinels.observe(newerSentinel);
  })();
})();
