// Bonfire client-side helpers, shared by every page (loaded in base.html).
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

const Bonfire = (() => {
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

    function start() {
        refreshTimes();
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

    return { formatRelative, formatLocal, refreshTimes, setupShareButtons, setupCollapsible, setupReplies };
})();
