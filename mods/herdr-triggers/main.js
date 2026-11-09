// Herdr Triggers: herdr-triggersd on herdr-core, seen from the app.
// One View, two renders. The daemon is a login service that
// `herdr-triggersd install` sets up; nothing here starts it on its own. As a
// sidebar block (props.strip) this is a status dot. As the dialog ctrl-alt-g
// opens it shows status and runs the daemon's commands. Every fact is one
// `herdr-triggersd --host core status --json`. Copies share only the state
// word and rule count: shared() is persisted to a file other local users can
// read, and the status carries log lines the daemon keeps owner-only.
import { View, div } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { exec, props, closeOverlay, theme, notify, run, layout, layouts, shared, setShared } from "herdr";

const BIN = "herdr-triggersd";
const HOST = ["--host", "core"];
const INSTALL = "cargo install --locked --git https://github.com/cantona/herdr-triggers";
const SETUP = "herdr-triggersd install";
const STATUS_KEY = "herdr-triggers.status";
// The block reads status once when it opens, often while the service is
// still waiting for herdr-core to come up beside the app; a few spaced
// re-reads let the dot settle without a standing timer.
const SETTLE_TRIES = 5;
const SETTLE_MS = 3000;
const BLOCK_ID = "herdr-triggers";
const SHIPPED = ["main", "terminals", "win98"];

const when = (ms) => new Date(ms).toLocaleTimeString();
const summary = (text) => {
  const lines = (text ?? "").split("\n").map((l) => l.trim()).filter(Boolean);
  return lines.find((l) => l.startsWith("error:")) ?? lines.slice(0, 3).join("\n");
};
// 0.1.x reads `--host` as the command and `core` as an unknown option.
const tooOld = (text) => /unknown option "core"|unknown command "--host"|unknown command "--follow-app"|unknown host/.test(text ?? "");

function findNode(node, id) {
  if (!node || typeof node !== "object") return null;
  if (node.id === id) return node;
  for (const child of node.children ?? []) {
    const found = findNode(child, id);
    if (found) return found;
  }
  return null;
}

function withoutNode(node, id) {
  if (!node || !Array.isArray(node.children)) return node;
  return { ...node, children: node.children.filter((c) => c.id !== id).map((c) => withoutNode(c, id)) };
}

// What the block needs, and all that leaves the dialog.
function brief(s) {
  if (s.install || s.error) return { word: "error", rules: null };
  const word = !s.data.running ? "stopped" : s.data.waiting ? "waiting" : "running";
  return { word, rules: s.data.config_error ? null : s.data.rules ?? 0 };
}

function serviceText(service) {
  if (!service?.installed) return `not installed; run ${SETUP}`;
  let text = `${service.manager}: installed`;
  if (service.this_core === false) text += ", watches the default core, not this one";
  // An older herdr-triggersd has no `reachable` field: only false says so.
  if (service.reachable === false) text += " (not reachable from here)";
  return text;
}

async function readStatus() {
  let r;
  try {
    r = await exec(BIN, [...HOST, "status", "--json"], { timeout: 15000 });
  } catch (e) {
    const error = String(e);
    return error.includes("cannot run") ? { install: "herdr-triggersd could not be started.", error } : { error };
  }
  if (r.code !== 0) {
    const output = r.stderr || r.stdout;
    return tooOld(output)
      ? { install: "herdr-triggersd is too old for this dialog.", error: summary(output) }
      : { error: summary(output) || `herdr-triggersd exited with code ${r.code}` };
  }
  try {
    return { data: JSON.parse(r.stdout) };
  } catch (e) {
    return { error: `unreadable status output: ${String(e)}` };
  }
}

export default class Triggers extends View {
  init(_props, cx) {
    this.strip = props()?.strip === true;
    this.busy = null;
    this.reading = false;
    this.error = null;
    this.status = null;
    if (this.strip) {
      cx.spawn(async (cx) => {
        await this.refresh(cx);
        for (let i = 0; i < SETTLE_TRIES && shared(STATUS_KEY)?.word === "waiting"; i++) {
          await cx.sleep(SETTLE_MS);
          await this.refresh(cx);
        }
      });
    } else {
      this.handle = cx.focus_handle();
      this.handle.focus();
      cx.spawn(async (cx) => this.refresh(cx));
    }
  }

  async refresh(cx) {
    this.reading = true;
    cx.notify();
    try {
      this.status = await readStatus();
      setShared(STATUS_KEY, brief(this.status));
    } finally {
      this.reading = false;
      cx.notify();
    }
  }

  act(cx, name, args) {
    if (this.busy || this.reading) return;
    this.busy = name;
    this.error = null;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        let r;
        try {
          r = await exec(BIN, [...HOST, ...args], { timeout: 120000 });
        } catch (e) {
          r = { code: 1, stderr: String(e), stdout: "" };
        }
        if (r.code !== 0) notify({ title: `Herdr Triggers: ${name} failed`, body: summary(r.stderr || r.stdout) || `exited with code ${r.code}`, level: "error" });
        await this.refresh(cx);
      } finally {
        this.busy = null;
        cx.notify();
      }
    });
  }

  // Two clicks when the shown layout is shipped: first clone it (the app
  // switches to the clone and re-renders), then insert the block. Shipped
  // layouts refuse structural edits, so there is no one-click path.
  sidebarAction() {
    const name = shared("layout");
    const tree = layout();
    if (findNode(tree, BLOCK_ID)) return { id: "sidebar-remove", text: "Remove from sidebar" };
    // Only a layout with a `sidebar` column can take the block; cloning one
    // without it would only leave the user on a copy.
    if (!findNode(tree, "sidebar")) return { id: "sidebar-none", text: "No sidebar in this layout" };
    if (SHIPPED.includes(name)) {
      // The clone already exists from an earlier press: go to it, since
      // cloning onto an existing name fails.
      const clone = `${name}-triggers`;
      if (layouts().includes(clone)) return { id: "sidebar-switch", text: `Switch to ${clone}`, clone };
      return { id: "sidebar-clone", text: "Clone layout for sidebar" };
    }
    return { id: "sidebar-add", text: "Add to sidebar" };
  }

  sidebar(cx) {
    const action = this.sidebarAction();
    const name = shared("layout");
    if (action.id === "sidebar-none") {
      this.error = "This layout has no sidebar column; see RECIPE.md to place the block by hand.";
      cx.notify();
      return;
    }
    if (action.id === "sidebar-switch") {
      run("layout.switch", { name: action.clone });
      cx.notify();
      return;
    }
    if (action.id === "sidebar-clone") {
      run("layout.clone", { name, as: `${name}-triggers` });
      cx.notify();
      return;
    }
    // A copy: the edited tree goes back through layout.set, never by mutating
    // what layout() returned.
    const tree = JSON.parse(JSON.stringify(layout()));
    if (action.id === "sidebar-remove") {
      run("layout.set", { layout: withoutNode(tree, BLOCK_ID) });
      cx.notify();
      return;
    }
    const sidebar = findNode(tree, "sidebar");
    if (!sidebar || !Array.isArray(sidebar.children)) {
      this.error = "This layout has no sidebar column; see RECIPE.md to place the block by hand.";
      cx.notify();
      return;
    }
    const block = { id: BLOCK_ID, type: "block", size: { px: 30 }, content: { mod: "herdr-triggers", props: { strip: true } } };
    const at = sidebar.children.findIndex((c) => c.id === "sidebar-footer");
    if (at >= 0) sidebar.children.splice(at, 0, block);
    else sidebar.children.push(block);
    run("layout.set", { layout: tree });
    cx.notify();
  }

  closeDialog() {
    closeOverlay(props().overlay);
  }

  stateWord(t, b) {
    if (!b) return ["reading", t.muted];
    const color = { running: t.working, waiting: t.idle, stopped: t.idle, error: t.blocked }[b.word] ?? t.muted;
    return [b.word, color];
  }

  button(id, text, primary, action) {
    const t = theme();
    return h_flex().id(id).h(30).px(12).items_center().rounded(8)
      .border_1().border_color(primary ? t.solid : t.input).bg(primary ? t.solid : t.raised)
      .text_color(primary ? t.solid_foreground : t.foreground).text_size(12.5).font_medium().cursor_pointer()
      .when(Boolean(this.busy || this.reading), (el) => el.opacity(0.6))
      .hover((s) => s.opacity(0.88))
      .on_click((_e, cx) => action(cx))
      .child(this.busy === id ? div().frames(["...", "..", "."], 300) : text);
  }

  renderStrip(cx) {
    const t = theme();
    const b = shared(STATUS_KEY);
    const [word, color] = this.stateWord(t, b);
    const rules = b?.rules ?? null;
    const narrow = (this.size?.width ?? 999) < 80;
    return h_flex().id("root").on_size_change((size, cx) => { this.size = size; cx.notify(); })
      .h(30).w_full().px(10).gap(8).items_center().cursor_pointer()
      .hover((st) => st.bg(t.raised))
      .on_click(() => run("mod.open", { name: "herdr-triggers" }))
      .child(div().w(8).h(8).rounded(4).bg(color))
      .when(!narrow, (el) => el
        .child(div().flex_1().truncate().text_size(12.5).text_color(t.foreground).child("Triggers"))
        .child(div().text_size(11.5).text_color(t.dim).child(rules === null ? word : `${rules} rules`)));
  }

  facts(t, d) {
    const row = (id, left, right, color) => h_flex().id(id).gap(8).pt(2).pb(2).items_center()
      .child(div().flex_1().truncate().text_color(t.foreground).child(left))
      .child(div().text_color(color ?? t.muted).child(right));
    const label = (text) => div().pt(12).pb(4).text_size(11.5).text_color(t.dim).child(text);
    const items = [label("Daemon")];
    const process = !d.running ? "not running" : d.waiting ? `pid ${d.pid}, waiting for herdr-core` : `pid ${d.pid}`;
    items.push(row("pid", "Process", process, d.running && !d.waiting ? t.working : t.idle));
    items.push(row("service", "Service", serviceText(d.service), d.service?.installed ? t.muted : t.blocked));
    items.push(row("socket", "herdr-core", d.socket));
    items.push(row("config", d.config, d.config_error ? "config error" : `${d.rules ?? 0} rules`, d.config_error ? t.blocked : t.muted));
    if (d.config_error) items.push(div().pl(12).text_color(t.blocked).child(d.config_error));
    if (d.last_fire) items.push(row("fire", `Last fire: ${d.last_fire.rule} in ${d.last_fire.pane}`, when(d.last_fire.at_ms)));
    items.push(label("Recent"));
    const log = (d.log ?? []).map((line, i) => div().id(`log-${i}`).text_size(11.5).font_family("monospace").text_color(t.muted).child(line));
    items.push(v_flex().max_h(220).overflow_y_scrollbar().children(log.length ? log : [div().text_color(t.dim).child("No log lines yet.")]));
    return v_flex().gap(2).children(items);
  }

  renderDialog(cx) {
    const t = theme();
    const s = this.status;
    let body;
    if (!s || this.reading && !s.data) body = div().text_color(t.muted).child("Reading...");
    else if (s.install) body = v_flex().gap(6)
      .child(div().text_color(t.blocked).child(s.install))
      .child(div().text_color(t.muted).child("Install the current version, then open this dialog again:"))
      .child(div().text_color(t.foreground).child(INSTALL))
      .child(div().text_color(t.foreground).child(SETUP))
      .child(div().text_size(11.5).text_color(t.dim).child(s.error ?? ""));
    else if (s.error) body = div().text_color(t.blocked).child(s.error);
    else body = this.facts(t, s.data);
    const running = Boolean(s?.data?.running);
    const side = this.sidebarAction();
    return v_flex().id("triggers").track_focus(this.handle)
      .on_key_down((e, cx) => { if (e.key === "escape") { this.closeDialog(); cx.stop_propagation(); } })
      .w_full().p(18).gap(4).rounded(8).bg(t.popover).text_size(13)
      .child(h_flex().items_center()
        .child(div().flex_1().text_size(15).font_semibold().text_color(t.foreground).child("Herdr Triggers"))
        .child(div().text_size(11).text_color(this.stateWord(t, s && brief(s))[1]).child(this.stateWord(t, s && brief(s))[0]))
        .child(div().pl(10).text_size(11).text_color(t.dim).child("Esc")))
      .child(div().max_h(460).overflow_y_scrollbar().child(body))
      .when(Boolean(this.error), (el) => el.child(div().pt(6).text_color(t.blocked).child(this.error)))
      .child(h_flex().pt(12).gap(8).flex_wrap().justify_end()
        .child(this.button(side.id, side.text, false, (cx) => this.sidebar(cx)))
        .child(this.button("reset", "Reset", false, (cx) => this.act(cx, "reset", ["reset"])))
        .child(this.button("reload", "Reload", false, (cx) => this.act(cx, "reload", ["reload"])))
        .child(this.button("restart", "Restart", false, (cx) => this.act(cx, "restart", ["restart"])))
        .child(running
          ? this.button("stop", "Stop", false, (cx) => this.act(cx, "stop", ["stop"]))
          : this.button("start", "Start", true, (cx) => this.act(cx, "start", ["start"]))));
  }

  render(cx) {
    return this.strip ? this.renderStrip(cx) : this.renderDialog(cx);
  }
}
