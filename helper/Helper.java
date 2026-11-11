package phantomqa;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.UiAutomation;
import android.graphics.Rect;
import android.os.Build;
import android.os.Bundle;
import android.os.HandlerThread;
import android.os.Looper;
import android.view.accessibility.AccessibilityNodeInfo;
import android.view.accessibility.AccessibilityWindowInfo;
import java.io.BufferedReader;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.InputStreamReader;
import java.io.PrintStream;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.concurrent.TimeoutException;
import org.json.JSONObject;

/**
 * PhantomQA's device helper. PhantomQA starts it over adb with app_process, as the shell user,
 * and stops it when the session ends. It installs nothing, opens no port, and talks only
 * through stdin and stdout:
 *
 *   in:  one JSON request per line:
 *          {"cmd": "dump"}                     the screen as XML, and where the keyboard is
 *          {"cmd": "type", "text": "..."}      put text at the cursor of the focused field
 *          {"cmd": "type", "text": "...", "replace": true}   make the text the field's whole text
 *          {"cmd": "end"}                      move the cursor to the end of the focused field
 *          {"cmd": "ping"}
 *   out: one reply per line: PQA {"ok": true, ...} or PQA {"ok": false, "error": "..."}
 *
 * It connects to the screen (UiAutomation) only for the moment of each request, so other tools
 * such as `uiautomator dump` keep working between requests.
 */
public final class Helper {
    /** Raise this when the requests or replies change. PhantomQA checks it at start. */
    static final int VERSION = 2;
    /** Wait for this long without screen changes before reading the screen. */
    static final long IDLE_MS = 300;
    /** Read the screen anyway after this long, for screens that never stop moving. */
    static final long IDLE_TIMEOUT_MS = 3000;
    static final int MAX_DEPTH = 100;
    /** UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES: keep TalkBack and others running. */
    static final int KEEP_SERVICES = 1;

    static HandlerThread thread;

    public static void main(String[] args) throws Exception {
        PrintStream out = new PrintStream(new FileOutputStream(FileDescriptor.out), true, "UTF-8");
        BufferedReader in = new BufferedReader(new InputStreamReader(System.in, "UTF-8"));
        thread = new HandlerThread("phantomqa");
        thread.start();
        out.println("PQA " + new JSONObject().put("ok", true).put("version", VERSION));
        String line;
        while ((line = in.readLine()) != null) {
            JSONObject reply;
            try {
                reply = handle(new JSONObject(line));
                reply.put("ok", true);
            } catch (Throwable e) {
                Throwable cause = e instanceof InvocationTargetException && e.getCause() != null ? e.getCause() : e;
                reply = new JSONObject().put("ok", false).put("error", String.valueOf(cause));
            }
            out.println("PQA " + reply);
        }
        // stdin closed: PhantomQA is gone.
        System.exit(0);
    }

    static JSONObject handle(JSONObject request) throws Exception {
        String cmd = request.getString("cmd");
        if (cmd.equals("ping")) {
            return new JSONObject();
        }
        UiAutomation automation = connect();
        try {
            if (cmd.equals("dump")) {
                return new JSONObject().put("xml", dump(automation));
            }
            if (cmd.equals("type")) {
                type(automation, request.getString("text"), request.optBoolean("replace", false));
                return new JSONObject();
            }
            if (cmd.equals("end")) {
                AccessibilityNodeInfo field = focusedField(automation);
                CharSequence text = field.getText();
                int end = text == null || showsHint(field) ? 0 : text.length();
                cursor(field, end);
                return new JSONObject();
            }
            throw new IllegalArgumentException("unknown cmd: " + cmd);
        } finally {
            disconnect(automation);
        }
    }

    // ------------------------------------------------------------ connecting

    static UiAutomation connect() throws Exception {
        Class<?> connectionClass = Class.forName("android.app.UiAutomationConnection");
        Constructor<?> newConnection = connectionClass.getDeclaredConstructor();
        newConnection.setAccessible(true);
        Constructor<UiAutomation> newAutomation =
                UiAutomation.class.getDeclaredConstructor(Looper.class, Class.forName("android.app.IUiAutomationConnection"));
        newAutomation.setAccessible(true);
        UiAutomation automation = newAutomation.newInstance(thread.getLooper(), newConnection.newInstance());
        try {
            Method connect = UiAutomation.class.getDeclaredMethod("connect", int.class);
            connect.setAccessible(true);
            connect.invoke(automation, KEEP_SERVICES);
        } catch (NoSuchMethodException e) {
            Method connect = UiAutomation.class.getDeclaredMethod("connect");
            connect.setAccessible(true);
            connect.invoke(automation);
        }
        AccessibilityServiceInfo info = automation.getServiceInfo();
        if (info != null) {
            // Like `uiautomator dump`: every view, with its resource id.
            // And the window list, to find the keyboard.
            info.flags |= AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS
                    | AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
                    | AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
            automation.setServiceInfo(info);
        }
        return automation;
    }

    static void disconnect(UiAutomation automation) {
        try {
            Method disconnect = UiAutomation.class.getDeclaredMethod("disconnect");
            disconnect.setAccessible(true);
            disconnect.invoke(automation);
        } catch (Throwable ignored) {
            // Gone already; the next request connects again.
        }
    }

    static AccessibilityNodeInfo root(UiAutomation automation) throws InterruptedException {
        for (int i = 0; i < 10; i++) {
            AccessibilityNodeInfo root = automation.getRootInActiveWindow();
            if (root != null) {
                return root;
            }
            Thread.sleep(100);
        }
        return null;
    }

    // ------------------------------------------------------------ reading the screen

    /** The screen as XML in the format of `uiautomator dump`, so PhantomQA reads both the same way. */
    static String dump(UiAutomation automation) throws Exception {
        try {
            automation.waitForIdle(IDLE_MS, IDLE_TIMEOUT_MS);
        } catch (TimeoutException e) {
            // Still moving, like a video or an animation: read it as it is.
        }
        StringBuilder xml = new StringBuilder("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>");
        xml.append("<hierarchy rotation=\"").append(rotation()).append('"');
        Rect keyboard = keyboard(automation);
        if (keyboard != null) {
            // Not in `uiautomator dump`: the app's elements under the keyboard cannot be touched.
            attr(xml, "keyboard", "[" + keyboard.left + "," + keyboard.top + "][" + keyboard.right + "," + keyboard.bottom + "]");
        }
        xml.append('>');
        AccessibilityNodeInfo root = root(automation);
        if (root != null) {
            Rect window = new Rect();
            root.getBoundsInScreen(window);
            node(xml, root, 0, 0, window);
        }
        return xml.append("</hierarchy>").toString();
    }

    static void node(StringBuilder xml, AccessibilityNodeInfo node, int index, int depth, Rect window) {
        Rect bounds = new Rect();
        node.getBoundsInScreen(bounds);
        if (!bounds.intersect(window)) {
            bounds.setEmpty();
        }
        CharSequence text = node.getText();
        CharSequence hint = null;
        if (Build.VERSION.SDK_INT >= 26) {
            hint = node.getHintText();
            // An empty field reports its hint as its text.
            if (node.isShowingHintText()) {
                text = null;
            }
        }
        xml.append("<node index=\"").append(index).append('"');
        attr(xml, "text", text);
        attr(xml, "resource-id", node.getViewIdResourceName());
        attr(xml, "class", node.getClassName());
        attr(xml, "package", node.getPackageName());
        attr(xml, "content-desc", node.getContentDescription());
        attr(xml, "checkable", node.isCheckable());
        attr(xml, "checked", node.isChecked());
        attr(xml, "clickable", node.isClickable());
        attr(xml, "enabled", node.isEnabled());
        attr(xml, "focusable", node.isFocusable());
        attr(xml, "focused", node.isFocused());
        attr(xml, "scrollable", node.isScrollable());
        attr(xml, "long-clickable", node.isLongClickable());
        attr(xml, "password", node.isPassword());
        attr(xml, "selected", node.isSelected());
        attr(xml, "bounds", "[" + bounds.left + "," + bounds.top + "][" + bounds.right + "," + bounds.bottom + "]");
        attr(xml, "hint", hint);
        int children = node.getChildCount();
        if (children == 0 || depth >= MAX_DEPTH) {
            xml.append(" />");
            return;
        }
        xml.append('>');
        for (int i = 0; i < children; i++) {
            AccessibilityNodeInfo child = node.getChild(i);
            // Like `uiautomator dump`: what a person cannot see is left out.
            if (child != null && child.isVisibleToUser()) {
                node(xml, child, i, depth + 1, window);
            }
        }
        xml.append("</node>");
    }

    static void attr(StringBuilder xml, String name, boolean value) {
        xml.append(' ').append(name).append("=\"").append(value).append('"');
    }

    static void attr(StringBuilder xml, String name, CharSequence value) {
        xml.append(' ').append(name).append("=\"");
        if (value != null) {
            for (int i = 0; i < value.length(); i++) {
                char c = value.charAt(i);
                switch (c) {
                    case '&': xml.append("&amp;"); break;
                    case '<': xml.append("&lt;"); break;
                    case '>': xml.append("&gt;"); break;
                    case '"': xml.append("&quot;"); break;
                    case '\'': xml.append("&apos;"); break;
                    default:
                        if (c < 0x20) {
                            xml.append("&#").append((int) c).append(';');
                        } else {
                            xml.append(c);
                        }
                }
            }
        }
        xml.append('"');
    }

    /** Where the on-screen keyboard is, or null when it is hidden. */
    static Rect keyboard(UiAutomation automation) {
        try {
            for (AccessibilityWindowInfo window : automation.getWindows()) {
                if (window.getType() == AccessibilityWindowInfo.TYPE_INPUT_METHOD) {
                    Rect bounds = new Rect();
                    window.getBoundsInScreen(bounds);
                    return bounds.isEmpty() ? null : bounds;
                }
            }
        } catch (Throwable e) {
            // No window list on this device.
        }
        return null;
    }

    /** 0 to 3, a quarter turn each. 0 when the window manager does not say. */
    static int rotation() {
        try {
            Object wm = Class.forName("android.view.WindowManagerGlobal").getMethod("getWindowManagerService").invoke(null);
            try {
                return (Integer) wm.getClass().getMethod("getDefaultDisplayRotation").invoke(wm);
            } catch (NoSuchMethodException e) {
                return (Integer) wm.getClass().getMethod("getRotation").invoke(wm);
            }
        } catch (Throwable e) {
            return 0;
        }
    }

    // ------------------------------------------------------------ typing

    static AccessibilityNodeInfo focusedField(UiAutomation automation) throws Exception {
        AccessibilityNodeInfo root = root(automation);
        AccessibilityNodeInfo field = root == null ? null : root.findFocus(AccessibilityNodeInfo.FOCUS_INPUT);
        if (field == null || !field.isEditable()) {
            throw new IllegalStateException("no text field has the focus");
        }
        return field;
    }

    /** An empty field reports its hint as its text. */
    static boolean showsHint(AccessibilityNodeInfo field) {
        return Build.VERSION.SDK_INT >= 26 && field.isShowingHintText();
    }

    static void cursor(AccessibilityNodeInfo field, int at) {
        Bundle cursor = new Bundle();
        cursor.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, at);
        cursor.putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, at);
        field.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, cursor);
    }

    /**
     * Puts `typed` into the focused field at the cursor, as if pasted, or makes it the whole text.
     * For text that `input text` cannot type (accents, other scripts, emoji), and for clearing a
     * field at once.
     */
    static void type(UiAutomation automation, String typed, boolean replace) throws Exception {
        AccessibilityNodeInfo field = focusedField(automation);
        CharSequence current = field.getText();
        // A password field reports dots, not its text; it is only ever replaced.
        String now = current == null || showsHint(field) || replace ? "" : current.toString();
        int start = field.getTextSelectionStart();
        int end = field.getTextSelectionEnd();
        if (start < 0 || start > now.length()) {
            start = now.length();
        }
        if (end < start || end > now.length()) {
            end = start;
        }
        Bundle text = new Bundle();
        text.putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE,
                now.substring(0, start) + typed + now.substring(end));
        if (!field.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, text)) {
            throw new IllegalStateException("the field did not take the text");
        }
        // The cursor goes after the new text, where typing would leave it.
        cursor(field, start + typed.length());
    }
}
