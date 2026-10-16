const { invoke } = window.__TAURI__.core;

const listEl = document.getElementById("todo-list");
const emptyEl = document.getElementById("empty");
const flashEl = document.getElementById("flash");
const showAllEl = document.getElementById("show-all");
const authStatusEl = document.getElementById("auth-status");

function flash(message, error = false) {
  flashEl.textContent = message;
  flashEl.className = error ? "error" : "";
  setTimeout(() => { flashEl.textContent = ""; }, 2500);
}

function dueLabel(todo) {
  const parts = todo.dueDate.split("-");
  const due = new Date(Number(parts[0]), Number(parts[1]) - 1, Number(parts[2]));
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const days = Math.round((due - today) / 86400000);
  const date = todo.dueDate;
  if (todo.completedOn) return { text: `done ${todo.completedOn}`, overdue: false };
  if (days < 0) return { text: `${date} (overdue)`, overdue: true };
  if (days === 0) return { text: "today", overdue: false };
  if (days === 1) return { text: "tomorrow", overdue: false };
  return { text: date, overdue: false };
}

function render(todos) {
  listEl.innerHTML = "";
  emptyEl.hidden = todos.length > 0;
  for (const todo of todos) {
    const li = document.createElement("li");
    li.className = todo.completedOn ? "done" : "";

    const check = document.createElement("input");
    check.type = "checkbox";
    check.checked = Boolean(todo.completedOn);
    check.addEventListener("change", async () => {
      try { render(await invoke("set_completed", { uuid: todo.uuid, completed: check.checked })); }
      catch (e) { flash(String(e), true); }
    });

    const desc = document.createElement("span");
    desc.className = "description";
    desc.textContent = todo.description;
    desc.title = "Double-click to edit";
    desc.addEventListener("dblclick", () => {
      const input = document.createElement("input");
      input.type = "text";
      input.value = todo.description;
      input.className = "edit";
      desc.replaceWith(input);
      input.focus();
      input.select();
      const commit = async () => {
        const value = input.value.trim();
        if (value && value !== todo.description) {
          try { render(await invoke("set_description", { uuid: todo.uuid, description: value })); return; }
          catch (e) { flash(String(e), true); }
        }
        input.replaceWith(desc);
      };
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") commit();
        if (event.key === "Escape") input.replaceWith(desc);
      });
      input.addEventListener("blur", commit);
    });

    const due = document.createElement("span");
    due.className = "due";
    const label = dueLabel(todo);
    due.textContent = label.text;
    if (label.overdue) due.classList.add("overdue");

    const del = document.createElement("button");
    del.className = "delete";
    del.textContent = "\u00d7";
    del.title = "Delete";
    del.addEventListener("click", async () => {
      try { render(await invoke("remove_todo", { uuid: todo.uuid })); }
      catch (e) { flash(String(e), true); }
    });

    li.append(check, desc, due, del);
    listEl.append(li);
  }
}

async function refresh() {
  try { render(await invoke("list_todos", { all: showAllEl.checked })); }
  catch (e) { flash(String(e), true); }
}

async function refreshAuth() {
  try {
    const status = await invoke("auth_status");
    authStatusEl.textContent = status.loggedIn ? "synced" : "local only";
  } catch { authStatusEl.textContent = ""; }
}

document.getElementById("add-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const input = document.getElementById("add-description");
  const value = input.value.trim();
  if (!value) return;
  const match = value.match(/^(.*?)\s+(\d+[dwmy]|\d{4}-\d{2}-\d{2})$/);
  let description = value;
  let due = null;
  if (match) { description = match[1]; due = match[2]; }
  try {
    render(await invoke("add_todo", { description, due }));
    input.value = "";
  } catch (e) { flash(String(e), true); }
});

showAllEl.addEventListener("change", refresh);

document.getElementById("sync-btn").addEventListener("click", async () => {
  try {
    const [pulled, pushed] = await invoke("sync_now");
    flash(`pulled ${pulled}, pushed ${pushed}`);
    refresh();
  } catch (e) { flash(String(e), true); }
});

document.getElementById("auth-btn").addEventListener("click", async () => {
  const status = await invoke("auth_status");
  if (status.loggedIn) {
    if (confirm("Log out?")) {
      try { await invoke("logout"); flash("logged out"); refreshAuth(); }
      catch (e) { flash(String(e), true); }
    }
    return;
  }
  const relay = prompt("Relay URL (e.g. https://relay.example.com):");
  if (!relay) return;
  try {
    await invoke("login", { relayUrl: relay });
    flash("logged in");
    refreshAuth();
  } catch (e) { flash(String(e), true); }
});

refresh();
refreshAuth();
