// Tison client-side helpers, shared by every page (loaded in base.html).
//
// Times: any `<time datetime="..." data-relative>` gets a relative text
// ("5 minutes ago", "3 days ago"...) refreshed while the page is open, and
// its local date and hour as a tooltip. Without JavaScript the text written
// by the server (in UTC) is kept.
//
// Links: any `<button data-share-url="/path" hidden>` is shown and opens the
// device's share menu (phones), or copies the full link to the clipboard.
//
// Long posts: any `<p data-collapsible id="...">` taller than the limit set in
// style.css (`--collapsed-lines`) is cut with a "Show more" button. Without
// JavaScript posts stay complete.
//
// Notifications: the unread count in the menu (`[data-unread]`) is kept up to
// date through a WebSocket to /notifications/live. On the notifications page
// new notifications appear at the top, read and deleted ones update at once,
// actions are sent without reloading, and older ones load when scrolling
// down. Without JavaScript the same links and forms reload the page.
//
// Replies: "Reply" links (`[data-reply-to]`) fill the comment form in place
// instead of reloading the page with `?reply_to=`.
//
// Deletions: forms with `data-confirm="Question?"` ask in a dialog of the
// site before being sent. Without JavaScript they are sent at once.
//
// Phone menu: the ☰ button (`[data-menu-toggle]`) opens and closes the menu
// (`.menu-open` on <html>, see style.css); the dimmed page behind it and
// Escape close it too.
//
// Messenger: the count next to "Messenger" follows the same WebSocket as the
// notifications; an open conversation fetches its new messages when told it
// changed, and Enter sends a message (Shift+Enter for a new line).
//
// Theme: the button under Settings (`[data-theme-toggle]`) switches between
// light and dark, remembered on this device. Until then the system's theme
// is followed. base.html applies the saved theme before the page is drawn.
//
// Search: from 3 letters typed in the search bar, the most relevant people
// and communities show under it (from /search/suggest), chosen with the
// mouse or the arrow keys and Enter. Without JavaScript, Enter opens the
// results page.

const Tison = (() => {
    // Same language as the interface, not the browser, so pages don't mix languages.
    const language = document.documentElement.lang || "en";

    // From the largest unit to the smallest: the first one that fits is used.
    // Months and years are averages, precise enough for "x months ago".
    const UNITS = [
        ["year", 365.25 * 24 * 60 * 60],
        ["month", 30.44 * 24 * 60 * 60],
        ["week", 7 * 24 * 60 * 60],
        ["day", 24 * 60 * 60],
        ["hour", 60 * 60],
        ["minute", 60],
        ["second", 1],
    ];

    const relativeFormat = new Intl.RelativeTimeFormat(language, { numeric: "always" });
    const localFormat = new Intl.DateTimeFormat(language, {
        dateStyle: "full",
        timeStyle: "medium",
    });

    // "5 minutes ago". Dates slightly in the future (clocks not in sync) count as now.
    function formatRelative(date, now = new Date()) {
        const seconds = Math.max(0, (now - date) / 1000);
        for (const [unit, unitSeconds] of UNITS) {
            if (seconds >= unitSeconds || unit === "second") {
                return relativeFormat.format(-Math.floor(seconds / unitSeconds), unit);
            }
        }
    }

    // "Friday, October 30, 2026 at 6:45:12 PM", in the viewer's time zone.
    function formatLocal(date) {
        return localFormat.format(date);
    }

    // Updates every relative time inside `root`.
    function refreshTimes(root = document) {
        const now = new Date();
        for (const element of root.querySelectorAll("time[datetime][data-relative]")) {
            const date = new Date(element.getAttribute("datetime"));
            if (Number.isNaN(date.getTime())) {
                continue;
            }
            element.textContent = formatRelative(date, now);
            element.title = formatLocal(date);
        }
    }

    // Full URL of a path on this site.
    function absoluteUrl(path) {
        return new URL(path, window.location.origin).href;
    }

    async function shareLink(button) {
        const url = absoluteUrl(button.dataset.shareUrl);
        if (navigator.share) {
            try {
                await navigator.share({ url });
            } catch {
                // Closed by the user, nothing to do.
            }
            return;
        }
        const label = button.textContent;
        try {
            await navigator.clipboard.writeText(url);
            button.textContent = "Link copied";
        } catch {
            // Clipboard refused (permissions, or not HTTPS): show the link to copy by hand.
            window.prompt("Copy this link:", url);
            return;
        }
        setTimeout(() => {
            button.textContent = label;
        }, 2000);
    }

    // Shows and activates the share buttons inside `root`, once each.
    function setupShareButtons(root = document) {
        for (const button of root.querySelectorAll("button[data-share-url]")) {
            if (button.dataset.shareReady) {
                continue;
            }
            button.dataset.shareReady = "true";
            if (navigator.share) {
                button.textContent = "Share link";
            }
            button.hidden = false;
            button.addEventListener("click", () => shareLink(button));
        }
    }

    // Collapses the long texts inside `root`, once each.
    function setupCollapsible(root = document) {
        for (const text of root.querySelectorAll("[data-collapsible]")) {
            if (text.dataset.collapseReady) {
                continue;
            }
            text.dataset.collapseReady = "true";
            text.classList.add("collapsed");
            // Short enough: nothing hidden, no button.
            if (text.scrollHeight <= text.clientHeight + 1) {
                text.classList.remove("collapsed");
                continue;
            }

            const button = document.createElement("button");
            button.type = "button";
            button.className = "action show-more";
            button.textContent = "Show more";
            button.setAttribute("aria-expanded", "false");
            button.setAttribute("aria-controls", text.id);
            button.addEventListener("click", () => {
                const collapsed = text.classList.toggle("collapsed");
                button.textContent = collapsed ? "Show more" : "Show less";
                button.setAttribute("aria-expanded", String(!collapsed));
            });
            text.after(button);
        }
    }

    function showUnread(unread) {
        const badge = document.querySelector("[data-notifications] [data-unread]");
        if (badge) {
            badge.firstChild.textContent = unread > 99 ? "99+" : String(unread);
            badge.hidden = unread === 0;
        }
        const readAll = document.querySelector("[data-read-all]");
        if (readAll) {
            readAll.hidden = unread === 0;
        }
    }

    /** The notifications list, `null` on other pages. */
    function notificationList() {
        return document.querySelector("[data-notification-list]");
    }

    /** Shows the empty message and hides "Delete all" when the list is empty. */
    function refreshEmptyState() {
        const list = notificationList();
        if (!list) {
            return;
        }
        const empty = list.children.length === 0;
        const message = document.querySelector("[data-notifications-empty]");
        if (message) {
            message.hidden = !empty;
        }
        const deleteAll = document.querySelector("[data-delete-all]");
        if (deleteAll) {
            deleteAll.hidden = empty;
        }
    }

    /** Turns server HTML into elements, ready to insert. */
    function parseItems(html) {
        const template = document.createElement("template");
        template.innerHTML = html;
        return [...template.content.children];
    }

    function markItemRead(item) {
        item.classList.remove("unread");
        item.querySelector("[data-unread-label]")?.setAttribute("hidden", "");
    }

    /** Applies a change sent by the server (see handlers/notification.rs). */
    function applyNotificationChange(change) {
        if (typeof change.unread === "number") {
            showUnread(change.unread);
        }
        if (typeof change.unread_messages === "number") {
            showUnreadMessages(change.unread_messages);
        }
        if (change.message) {
            refreshConversation(change.message);
        }
        const list = notificationList();
        if (!list) {
            return;
        }
        if (change.added && list.hasAttribute("data-newest")) {
            // Already shown, from a reconnection: replaced by the fresh version.
            document.getElementById(`notification-${change.added.id}`)?.remove();
            for (const item of parseItems(change.added.html)) {
                item.classList.add("arriving");
                list.prepend(item);
                refreshTimes(item);
            }
        }
        if (change.removed === "all") {
            list.replaceChildren();
        } else if (change.removed) {
            document.getElementById(`notification-${change.removed}`)?.remove();
        }
        if (change.read === "all") {
            list.querySelectorAll(".notification.unread").forEach(markItemRead);
        } else if (change.read) {
            const item = document.getElementById(`notification-${change.read}`);
            if (item) {
                markItemRead(item);
            }
        }
        refreshEmptyState();
    }

    /**
     * Keeps notifications up to date: the server sends `{"unread": n}` when
     * connecting, then every change. Reconnects after a lost connection,
     * waiting longer each time (up to a minute).
     */
    function connectNotifications() {
        const badge = document.querySelector("[data-notifications] [data-unread]");
        // Only signed-in pages have a count.
        if (!badge || !("WebSocket" in window)) {
            return;
        }
        const address = new URL("/notifications/live", location.href);
        address.protocol = location.protocol === "https:" ? "wss:" : "ws:";
        let delay = 1000;
        let socket = null;

        function open() {
            socket = new WebSocket(address);
            socket.addEventListener("open", () => {
                delay = 1000;
            });
            socket.addEventListener("message", (event) => {
                let change;
                try {
                    change = JSON.parse(event.data);
                } catch {
                    return;
                }
                applyNotificationChange(change);
            });
            socket.addEventListener("close", () => {
                socket = null;
                // Hidden tabs reconnect when shown again, see below.
                if (!document.hidden) {
                    setTimeout(open, delay);
                    delay = Math.min(delay * 2, 60 * 1000);
                }
            });
        }

        document.addEventListener("visibilitychange", () => {
            if (!document.hidden && socket === null) {
                delay = 1000;
                open();
            }
        });
        open();
    }

    /**
     * Notifications page: actions sent in the background, applied at once
     * (the server confirms through the WebSocket), and older notifications
     * loaded when "Older notifications" comes into view.
     */
    function setupNotificationsPage() {
        const page = document.querySelector("[data-notifications-page]");
        if (!page) {
            return;
        }

        page.addEventListener("submit", async (event) => {
            const form = event.target.closest("[data-notification-action]");
            if (!form) {
                return;
            }
            event.preventDefault();
            const button = form.querySelector("button");
            button.disabled = true;
            try {
                const response = await fetch(form.action, { method: "POST", credentials: "same-origin" });
                if (!response.ok) {
                    throw new Error(String(response.status));
                }
                // Applied here too, in case the WebSocket is down.
                const action = new URL(form.action).pathname;
                if (action === "/notifications/read-all") {
                    applyNotificationChange({ read: "all", unread: 0 });
                } else if (action === "/notifications/delete-all") {
                    applyNotificationChange({ removed: "all", unread: 0 });
                } else {
                    form.closest("[data-notification-id]")?.remove();
                    refreshEmptyState();
                }
            } catch {
                // The normal way: reloads the page with the result or an error.
                form.submit();
            } finally {
                button.disabled = false;
            }
        });

        // Opening one marks it read at once; the link itself does it on the server.
        page.addEventListener("click", (event) => {
            const item = event.target.closest(".notification.unread");
            if (item && event.target.closest(".notification-link")) {
                markItemRead(item);
            }
        });

        const more = page.querySelector("[data-load-older]");
        if (!more || !("IntersectionObserver" in window)) {
            return;
        }
        let loading = false;
        const observer = new IntersectionObserver(async (entries) => {
            if (loading || !entries.some((entry) => entry.isIntersecting)) {
                return;
            }
            loading = true;
            try {
                const response = await fetch(`/notifications/older?before=${more.dataset.loadOlder}`, {
                    credentials: "same-origin",
                });
                if (!response.ok) {
                    throw new Error(String(response.status));
                }
                const { html, next_before: next } = await response.json();
                const list = notificationList();
                for (const item of parseItems(html)) {
                    // May already be there if it was added live meanwhile.
                    if (!document.getElementById(item.id)) {
                        list.append(item);
                        refreshTimes(item);
                    }
                }
                if (next) {
                    more.dataset.loadOlder = next;
                    more.href = `/notifications?before=${next}`;
                } else {
                    observer.disconnect();
                    more.remove();
                }
                refreshEmptyState();
            } catch {
                // The link stays: clicking it loads the next page.
                observer.disconnect();
            } finally {
                loading = false;
            }
        });
        observer.observe(more);
    }

    /** "Reply" links fill the comment form instead of reloading the page. */
    function setupReplies(root = document) {
        const form = root.querySelector("#comment-form");
        if (!form) {
            return;
        }
        const field = form.querySelector("[data-reply-field]");
        const banner = form.querySelector("[data-reply-banner]");
        const name = form.querySelector("[data-reply-name]");
        const text = form.querySelector("textarea");

        for (const link of root.querySelectorAll("[data-reply-to]")) {
            link.addEventListener("click", (event) => {
                event.preventDefault();
                field.value = link.dataset.replyTo;
                name.textContent = "@" + link.dataset.replyAuthor;
                banner.hidden = false;
                form.scrollIntoView({ behavior: "smooth", block: "center" });
                text.focus({ preventScroll: true });
            });
        }
        form.querySelector("[data-reply-cancel]")?.addEventListener("click", (event) => {
            event.preventDefault();
            field.value = "";
            banner.hidden = true;
            text.focus();
        });
    }

    /** The ☰ button of the top bar on phones. */
    function setupMenu() {
        const button = document.querySelector("[data-menu-toggle]");
        const menu = document.getElementById("site-menu");
        if (!button || !menu) {
            return;
        }
        const root = document.documentElement;
        const isOpen = () => root.classList.contains("menu-open");

        function toggle(open) {
            root.classList.toggle("menu-open", open);
            button.setAttribute("aria-expanded", String(open));
            button.setAttribute("aria-label", open ? "Close the menu" : "Open the menu");
            if (open) {
                menu.querySelector("nav a")?.focus();
            }
        }

        button.addEventListener("click", () => toggle(!isOpen()));
        document.querySelector("[data-menu-close]")?.addEventListener("click", () => toggle(false));
        document.addEventListener("keydown", (event) => {
            if (event.key === "Escape" && isOpen()) {
                toggle(false);
                button.focus();
            }
        });
        // Back to a wide screen: the menu is a column again, nothing to close.
        window.matchMedia("(min-width: 50.01em)").addEventListener("change", (event) => {
            if (event.matches) {
                toggle(false);
            }
        });
    }

    /** The count next to "Messenger", hidden at 0. */
    function showUnreadMessages(count) {
        const badge = document.querySelector("[data-unread-messages]");
        if (!badge) {
            return;
        }
        badge.hidden = count === 0;
        badge.firstChild.textContent = count > 99 ? "99+" : String(count);
    }

    /**
     * A conversation changed (`id`): the open conversation gets its new
     * messages, edits and deletions; the list of conversations is reloaded.
     */
    async function refreshConversation(id) {
        const page = document.querySelector(`[data-conversation="${id}"]`);
        if (!page) {
            const list = document.querySelector(".conversation-list");
            if (list) {
                try {
                    const response = await fetch("/messenger", { credentials: "same-origin" });
                    const fresh = new DOMParser().parseFromString(await response.text(), "text/html").querySelector(".conversation-list");
                    if (fresh) {
                        list.replaceWith(fresh);
                        refreshTimes(fresh);
                    }
                } catch {
                    // Up to date at the next change or reload.
                }
            }
            return;
        }
        const messages = page.querySelector("[data-message-list]");
        const shown = [...messages.querySelectorAll("[data-message-id]")];
        const last = shown.length ? shown[shown.length - 1].dataset.messageId : 0;
        let found;
        try {
            const response = await fetch(`/messenger/${id}/after/${last}`, { credentials: "same-origin" });
            if (!response.ok) {
                return;
            }
            found = await response.json();
        } catch {
            return;
        }
        // Read before the changes: at the bottom, the new ones stay in view.
        const scroller = conversationScroller(messages);
        const atBottom = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 80;
        const kept = new Set(found.ids.map(String));
        for (const item of shown) {
            if (!kept.has(item.dataset.messageId)) {
                item.remove();
            } else if (found.bodies[item.dataset.messageId] !== undefined) {
                // Edited on the other server.
                const body = item.querySelector("[data-message-body]");
                if (body.innerHTML !== found.bodies[item.dataset.messageId]) {
                    body.innerHTML = found.bodies[item.dataset.messageId];
                }
            }
        }
        const template = document.createElement("template");
        template.innerHTML = found.html;
        for (const item of [...template.content.children]) {
            if (!document.getElementById(item.id)) {
                item.classList.add("arriving");
                messages.append(item);
                refreshTimes(item);
            }
        }
        page.querySelector("[data-messages-empty]").hidden = messages.children.length > 0;
        if (atBottom) {
            scroller.scrollTop = scroller.scrollHeight;
        }
    }

    /**
     * What scrolls in a conversation: its message list, which fills the
     * window (see style.css), else the page (browsers without `:has`).
     */
    function conversationScroller(messages) {
        return messages.scrollHeight > messages.clientHeight ? messages : document.scrollingElement;
    }

    /** The conversation page: shows the latest messages; Enter sends. */
    function setupConversation() {
        const page = document.querySelector("[data-conversation]");
        if (!page) {
            return;
        }
        // The latest messages in view.
        const messages = page.querySelector("[data-message-list]");
        const scroller = conversationScroller(messages);
        scroller.scrollTop = scroller.scrollHeight;
        const input = page.querySelector("[data-message-input]");
        input?.addEventListener("keydown", (event) => {
            // Shift+Enter (or a phone's keyboard) makes a new line.
            if (event.key === "Enter" && !event.shiftKey && !event.isComposing && input.value.trim()) {
                event.preventDefault();
                input.form.requestSubmit();
            }
        });
        if (location.hash === "#composer" || location.hash.startsWith("#message-")) {
            input?.focus();
        }
    }

    /** The light / dark button of the menu. */
    function setupTheme() {
        const button = document.querySelector("[data-theme-toggle]");
        if (!button) {
            return;
        }
        const root = document.documentElement;
        const system = window.matchMedia("(prefers-color-scheme: dark)");
        const KEY = "tison-theme";
        const saved = () => {
            try {
                return localStorage.getItem(KEY);
            } catch {
                return null;
            }
        };
        const current = () => root.dataset.theme || (system.matches ? "dark" : "light");

        function show() {
            const dark = current() === "dark";
            // The button names the theme it switches to.
            button.querySelector("[data-theme-label]").textContent = dark ? "Light mode" : "Dark mode";
            button.setAttribute("aria-pressed", String(dark));
            button.title = dark ? "Switch to the light theme" : "Switch to the dark theme";
        }

        function apply(theme) {
            root.dataset.theme = theme;
            show();
        }

        button.addEventListener("click", () => {
            const theme = current() === "dark" ? "light" : "dark";
            try {
                localStorage.setItem(KEY, theme);
            } catch {
                // Not remembered (private browsing...), applied anyway.
            }
            apply(theme);
        });
        // Other tabs follow, and the system's theme while none is chosen.
        window.addEventListener("storage", (event) => {
            if (event.key === KEY && (event.newValue === "light" || event.newValue === "dark")) {
                apply(event.newValue);
            }
        });
        system.addEventListener("change", () => {
            if (!saved()) {
                show();
            }
        });
        // Without a choice, `data-theme` stays unset: the CSS follows the system.
        show();
        button.hidden = false;
    }

    /** Suggestions under the search bar while typing. */
    function setupSearch() {
        const form = document.querySelector(".site-search");
        const input = form?.querySelector("input[name=q]");
        if (!input) {
            return;
        }
        const list = document.createElement("ul");
        list.className = "search-suggestions";
        list.id = "search-suggestions";
        list.setAttribute("role", "listbox");
        list.hidden = true;
        form.append(list);
        input.setAttribute("autocomplete", "off");
        input.setAttribute("aria-autocomplete", "list");
        input.setAttribute("aria-controls", list.id);
        input.setAttribute("aria-expanded", "false");

        // Only the answer to the latest text is shown.
        let asked = 0;
        let timer = null;

        const options = () => [...list.querySelectorAll("a")];
        const close = () => {
            list.hidden = true;
            input.setAttribute("aria-expanded", "false");
        };

        function select(index) {
            const all = options();
            all.forEach((option, position) => option.setAttribute("aria-selected", String(position === index)));
            all[index]?.scrollIntoView({ block: "nearest" });
        }

        function entry(item) {
            const link = document.createElement("a");
            link.href = item.url;
            link.setAttribute("role", "option");
            link.setAttribute("aria-selected", "false");
            if (item.photo) {
                const image = document.createElement("img");
                image.src = item.photo;
                image.alt = "";
                image.className = "nav-avatar";
                link.append(image);
            } else {
                const initial = document.createElement("span");
                initial.className = "nav-avatar placeholder";
                initial.setAttribute("aria-hidden", "true");
                initial.textContent = item.initial;
                link.append(initial);
            }
            const name = document.createElement("strong");
            name.textContent = item.name;
            const detail = document.createElement("span");
            detail.className = "detail";
            detail.textContent = item.detail;
            link.append(name, detail);
            const row = document.createElement("li");
            row.append(link);
            return row;
        }

        function show(text, found) {
            list.replaceChildren();
            for (const [label, items] of [["People", found.users], ["Communities", found.communities]]) {
                if (!items.length) {
                    continue;
                }
                const group = document.createElement("li");
                group.className = "group";
                group.setAttribute("role", "presentation");
                group.textContent = label;
                list.append(group, ...items.map(entry));
            }
            if (!list.children.length) {
                close();
                return;
            }
            const all = document.createElement("a");
            all.href = "/search?q=" + encodeURIComponent(text);
            all.className = "all";
            all.setAttribute("role", "option");
            all.textContent = `All results for “${text}”`;
            const row = document.createElement("li");
            row.append(all);
            list.append(row);
            list.hidden = false;
            input.setAttribute("aria-expanded", "true");
        }

        async function ask(text) {
            const number = ++asked;
            try {
                const response = await fetch("/search/suggest?q=" + encodeURIComponent(text), {
                    headers: { Accept: "application/json" },
                });
                if (!response.ok) {
                    return;
                }
                const found = await response.json();
                if (number === asked && input.value.trim() === text) {
                    show(text, found);
                }
            } catch {
                // No suggestions: Enter still searches.
            }
        }

        input.addEventListener("input", () => {
            clearTimeout(timer);
            const text = input.value.trim();
            if (text.replace(/^[@!]/, "").length < 3) {
                asked++;
                close();
                return;
            }
            timer = setTimeout(() => ask(text), 150);
        });

        input.addEventListener("keydown", (event) => {
            if (list.hidden) {
                return;
            }
            const all = options();
            const current = all.findIndex((option) => option.getAttribute("aria-selected") === "true");
            if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                event.preventDefault();
                const step = event.key === "ArrowDown" ? 1 : -1;
                select((current + step + all.length) % all.length);
            } else if (event.key === "Enter" && current >= 0) {
                event.preventDefault();
                window.location.href = all[current].href;
            } else if (event.key === "Escape") {
                close();
            }
        });

        // Clicking a suggestion must not blur the field first.
        list.addEventListener("mousedown", (event) => event.preventDefault());
        input.addEventListener("blur", close);
        input.addEventListener("focus", () => {
            if (list.children.length && input.value.trim().length >= 3) {
                list.hidden = false;
                input.setAttribute("aria-expanded", "true");
            }
        });
    }

    /**
     * The dialog asking before a deletion, made once: the question, "Cancel"
     * and the confirming button.
     */
    function confirmDialog() {
        let dialog = document.getElementById("confirm-dialog");
        if (dialog) {
            return dialog;
        }
        dialog = document.createElement("dialog");
        dialog.id = "confirm-dialog";
        dialog.className = "confirm-dialog";
        dialog.setAttribute("aria-labelledby", "confirm-dialog-text");
        dialog.innerHTML = `
            <form method="dialog">
                <p id="confirm-dialog-text" class="confirm-dialog-text"></p>
                <div class="confirm-dialog-actions">
                    <button type="submit" value="cancel" class="button" data-confirm-cancel>Cancel</button>
                    <button type="submit" value="confirm" class="button danger-filled" data-confirm-ok></button>
                </div>
            </form>`;
        // A click on the dimmed page around it cancels.
        dialog.addEventListener("click", (event) => {
            if (event.target === dialog) {
                dialog.close("cancel");
            }
        });
        document.body.append(dialog);
        return dialog;
    }

    /**
     * Forms with `data-confirm="Question?"` ask before deleting something, in
     * a dialog of the site. The confirming button says what the form's button
     * says ("Delete", "Remove"), or `data-confirm-label`. One listener for the
     * page: forms added later (live messages) ask too.
     */
    function setupConfirmations() {
        if (!("HTMLDialogElement" in window)) {
            // Very old browsers: their own box.
            document.addEventListener("submit", (event) => {
                const form = event.target.closest("form[data-confirm]");
                if (form && !window.confirm(form.dataset.confirm)) {
                    event.preventDefault();
                }
            });
            return;
        }
        document.addEventListener("submit", (event) => {
            const form = event.target.closest("form[data-confirm]");
            if (!form) {
                return;
            }
            // Asked already: sent for real.
            if (form.dataset.confirmed === "true") {
                delete form.dataset.confirmed;
                return;
            }
            event.preventDefault();
            const submitter = event.submitter;
            const dialog = confirmDialog();
            dialog.querySelector(".confirm-dialog-text").textContent = form.dataset.confirm;
            dialog.querySelector("[data-confirm-ok]").textContent =
                form.dataset.confirmLabel || submitter?.textContent.trim() || "Confirm";
            dialog.returnValue = "";
            dialog.addEventListener(
                "close",
                () => {
                    if (dialog.returnValue === "confirm") {
                        form.dataset.confirmed = "true";
                        form.requestSubmit(submitter && form.contains(submitter) ? submitter : undefined);
                    } else {
                        submitter?.focus();
                    }
                },
                { once: true },
            );
            dialog.showModal();
            // The safe choice first: Enter doesn't delete by accident.
            dialog.querySelector("[data-confirm-cancel]").focus();
        });
    }

    function start() {
        refreshTimes();
        setupConfirmations();
        setupSearch();
        setupTheme();
        setupMenu();
        setupConversation();
        setupShareButtons();
        setupCollapsible();
        setupReplies();
        setupNotificationsPage();
        connectNotifications();
        // Often enough for "x seconds ago" to move, cheap even with many posts.
        setInterval(refreshTimes, 10 * 1000);
    }

    if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", start);
    } else {
        start();
    }

    return { formatRelative, formatLocal, refreshTimes, setupShareButtons, setupCollapsible, setupReplies, setupConfirmations, setupSearch, setupTheme, setupMenu };
})();
