#ifndef TRAY_H
#define TRAY_H

/* Max time to block in tray_loop() */
#define MAX_WAIT_SECONDS 5

struct tray_menu;

struct tray
{
  char *icon;
  struct tray_menu *menu;
};

struct tray_menu
{
  char *text;
  wchar_t *wtext; /* Windows only */
  int disabled;
  int checked;

  void (*cb)(struct tray_menu *);

  void *context;

  struct tray_menu *submenu;
};

void tray_update(struct tray *tray);

/* Make a blocking tray_loop() return as soon as possible. Safe to call from
 * any thread. */
void tray_wake(void);

#if defined(TRAY_APPINDICATOR)

#include <gtk/gtk.h>
#include <libayatana-appindicator/app-indicator.h>

#define TRAY_APPINDICATOR_ID "tray-id"

static AppIndicator *indicator = NULL;
static int loop_result = 0;

static void _tray_menu_cb(GtkMenuItem *item, gpointer data) {
  (void)item;
  struct tray_menu *m = (struct tray_menu *)data;
  m->cb(m);
}

static GtkMenuShell *_tray_menu(struct tray_menu *m) {
  GtkMenuShell *menu = (GtkMenuShell *)gtk_menu_new();
  for (; m != NULL && m->text != NULL; m++) {
    GtkWidget *item;
    if (strcmp(m->text, "-") == 0) {
      item = gtk_separator_menu_item_new();
    } else {
      if (m->submenu != NULL) {
        item = gtk_menu_item_new_with_label(m->text);
        gtk_menu_item_set_submenu(GTK_MENU_ITEM(item),
                                  GTK_WIDGET(_tray_menu(m->submenu)));
      } else {
        item = gtk_check_menu_item_new_with_label(m->text);
        gtk_check_menu_item_set_active(GTK_CHECK_MENU_ITEM(item), !!m->checked);
      }
      gtk_widget_set_sensitive(item, !m->disabled);
      if (m->cb != NULL) {
        g_signal_connect(item, "activate", G_CALLBACK(_tray_menu_cb), m);
      }
    }
    gtk_widget_show(item);
    gtk_menu_shell_append(menu, item);
  }
  return menu;
}

int tray_init(struct tray *tray) {
  if (gtk_init_check(0, NULL) == FALSE) {
    return -1;
  }
  indicator = app_indicator_new(TRAY_APPINDICATOR_ID, tray->icon,
                                APP_INDICATOR_CATEGORY_APPLICATION_STATUS);
  app_indicator_set_status(indicator, APP_INDICATOR_STATUS_ACTIVE);
  tray_update(tray);
  return 0;
}

int tray_loop(int blocking) {
  gtk_main_iteration_do(blocking);
  return loop_result;
}

void tray_update(struct tray *tray) {
  app_indicator_set_icon(indicator, tray->icon);
  // GTK is all about reference counting, so previous menu should be destroyed
  // here
  app_indicator_set_menu(indicator, GTK_MENU(_tray_menu(tray->menu)));
}

void tray_exit() { loop_result = -1; }

/* Runs on the GTK main loop; only exists so gtk_main_iteration_do() returns. */
static gboolean _tray_wake_cb(gpointer data) {
  (void)data;
  return G_SOURCE_REMOVE;
}

/* g_idle_add() is thread safe and wakes up the default main context. */
void tray_wake(void) { g_idle_add(_tray_wake_cb, NULL); }

#elif defined(TRAY_APPKIT)

#include <objc/objc-runtime.h>
#include <dispatch/dispatch.h>
#include <limits.h>

/* Same layout as CGPoint/NSPoint (two doubles), which is not declared by the
 * headers included here. */
typedef struct { double x; double y; } tray_point;

/* NSEventTypeApplicationDefined */
#define TRAY_NS_EVENT_TYPE_APPLICATION_DEFINED 15UL

static id app = NULL;
static id statusBar = NULL;
static id statusItem = NULL;
static id statusBarButton = NULL;
static id rootMenu = NULL;
static id kCFRunLoopDefaultMode = NULL;

static id _tray_menu(struct tray_menu *m, id menu) {
    if (menu) {
      ((void(*)(id, SEL))objc_msgSend)(menu, sel_registerName("removeAllItems"));
    } else {
      menu = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSMenu"), sel_registerName("new"));
      menu = ((id(*)(id, SEL))objc_msgSend)(menu, sel_registerName("autorelease"));
      ((void(*)(id, SEL, bool))objc_msgSend)(menu, sel_registerName("setAutoenablesItems:"), false);
    }

    for (; m != NULL && m->text != NULL; m++) {
        if (strcmp(m->text, "-") == 0) {
            /* separatorItem already returns an autoreleased object; releasing
             * it once more would free it while the menu still holds it. */
            id menuItem = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSMenuItem"), sel_registerName("separatorItem"));
            ((void(*)(id, SEL, id))objc_msgSend)(menu, sel_registerName("addItem:"), menuItem);
        } else {
            id menuItem = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSMenuItem"), sel_registerName("alloc"));
            ((void(*)(id, SEL))objc_msgSend)(menuItem, sel_registerName("autorelease"));
            ((void(*)(id, SEL, id, SEL, id))objc_msgSend)(
              menuItem,
              sel_registerName("initWithTitle:action:keyEquivalent:"),
              ((id(*)(id, SEL, char *))objc_msgSend)((id)objc_getClass("NSString"), sel_registerName("stringWithUTF8String:"), m->text),
              sel_registerName("menuCallback:"),
              ((id(*)(id, SEL, char *))objc_msgSend)((id)objc_getClass("NSString"), sel_registerName("stringWithUTF8String:"), ""));
            ((void(*)(id, SEL, bool))objc_msgSend)(menuItem, sel_registerName("setEnabled:"), (m->disabled ? false : true));
            ((void(*)(id, SEL, int))objc_msgSend)(menuItem, sel_registerName("setState:"), (m->checked ? 1 : 0));
            ((void(*)(id, SEL, id))objc_msgSend)(menuItem, sel_registerName("setRepresentedObject:"),
              ((id(*)(id, SEL, struct tray_menu*))objc_msgSend)((id)objc_getClass("NSValue"), sel_registerName("valueWithPointer:"), m));
            ((void(*)(id, SEL, id))objc_msgSend)(menu, sel_registerName("addItem:"), menuItem);
            if (m->submenu != NULL) {
                ((void(*)(id, SEL, id, id))objc_msgSend)(menu, sel_registerName("setSubmenu:forItem:"), _tray_menu(m->submenu, NULL), menuItem);
            }
        }
    }

    return menu;
}

static void menu_callback(id self, SEL cmd, id sender) {
    id asdf = ((id(*)(id, SEL))objc_msgSend)(sender, sel_registerName("representedObject"));
    struct tray_menu *m = ((struct tray_menu*(*)(id, SEL))objc_msgSend)(asdf,sel_registerName("pointerValue"));
    if (m != NULL && m->cb != NULL) {
        m->cb(m);
    }
}

int tray_init(struct tray *tray) {
    kCFRunLoopDefaultMode = ((id(*)(id, SEL, char*))objc_msgSend)((id)objc_getClass("NSString"), sel_registerName("stringWithUTF8String:"), "kCFRunLoopDefaultMode");
    /* stringWithUTF8String: returns an autoreleased object; keep our own
     * reference since it lives in a static for the lifetime of the process. */
    ((void(*)(id, SEL))objc_msgSend)(kCFRunLoopDefaultMode, sel_registerName("retain"));

    ((void(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSApplication"), sel_registerName("sharedApplication"));
    Class trayDelegateClass = objc_allocateClassPair(objc_getClass("NSObject"), "Tray", 0);
    class_addProtocol(trayDelegateClass, objc_getProtocol("NSApplicationDelegate"));
    class_addMethod(trayDelegateClass, sel_registerName("menuCallback:"), (IMP)menu_callback, "v@:@");
    objc_registerClassPair(trayDelegateClass);
    id trayDelegate = ((id(*)(id, SEL))objc_msgSend)((id)trayDelegateClass, sel_registerName("new"));

    app = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSApplication"), sel_registerName("sharedApplication"));
    ((void(*)(id, SEL,id))objc_msgSend)(app, sel_registerName("setDelegate:"), trayDelegate);

    statusBar = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSStatusBar"), sel_registerName("systemStatusBar"));
    statusItem = ((id(*)(id, SEL, double))objc_msgSend)(statusBar, sel_registerName("statusItemWithLength:"), -1.0);
    ((void(*)(id, SEL))objc_msgSend)(statusItem, sel_registerName("retain"));
    ((void(*)(id, SEL, bool))objc_msgSend)(statusItem, sel_registerName("setHighlightMode:"), true);
    statusBarButton = ((id(*)(id, SEL))objc_msgSend)(statusItem, sel_registerName("button"));

    tray_update(tray);
    /*((void(*)(id, SEL, bool))objc_msgSend)(app, sel_registerName("activateIgnoringOtherApps:"), true);*/
    return 0;
}

int tray_loop(int blocking) {
    id pool = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSAutoreleasePool"), sel_registerName("new"));

    id until;
    if (blocking) {
        until = ((id(*)(id, SEL, double))objc_msgSend)((id)objc_getClass("NSDate"), sel_registerName("dateWithTimeIntervalSinceNow:"), (double)MAX_WAIT_SECONDS);
    } else {
        until = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSDate"), sel_registerName("distantPast"));
    }

    id event = ((id(*)(id, SEL, unsigned long, id, id, bool))objc_msgSend)(app, sel_registerName("nextEventMatchingMask:untilDate:inMode:dequeue:"), ULONG_MAX, until, kCFRunLoopDefaultMode, true);
    if (event) {
        ((void(*)(id, SEL, id))objc_msgSend)(app, sel_registerName("sendEvent:"), event);
    }

    ((void(*)(id, SEL))objc_msgSend)(pool, sel_registerName("drain"));

    return 0;
}

void tray_update(struct tray *tray) {
    /* tray_update() is called from outside tray_loop()'s pool, so without a
     * pool of its own every autoreleased menu object built here would leak. */
    id pool = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSAutoreleasePool"), sel_registerName("new"));

    if (tray->icon) {
        ((void(*)(id, SEL, id))objc_msgSend)(statusBarButton, sel_registerName("setImage:"), ((id(*)(id, SEL, id))objc_msgSend)((id)objc_getClass("NSImage"), sel_registerName("imageNamed:"), ((id(*)(id, SEL, char *))objc_msgSend)((id)objc_getClass("NSString"), sel_registerName("stringWithUTF8String:"), tray->icon)));
    }
    id wasMenuSet = rootMenu;
    rootMenu = _tray_menu(tray->menu, rootMenu);
    if (!wasMenuSet) {
      /* The root menu is created autoreleased; the static needs its own
       * reference so it does not depend on setMenu: retaining it. */
      ((void(*)(id, SEL))objc_msgSend)(rootMenu, sel_registerName("retain"));
      ((void(*)(id, SEL, id))objc_msgSend)(statusItem, sel_registerName("setMenu:"), rootMenu);
    }

    ((void(*)(id, SEL))objc_msgSend)(pool, sel_registerName("drain"));
}

/* Runs on the main thread. Posting an application-defined event makes a
 * nextEventMatchingMask: call in tray_loop() return right away; the dispatch
 * itself only wakes the run loop without ending that call. */
static void _tray_wake_main(void *context) {
    (void)context;
    if (!app)
        return;
    id pool = ((id(*)(id, SEL))objc_msgSend)((id)objc_getClass("NSAutoreleasePool"), sel_registerName("new"));
    tray_point location = { 0.0, 0.0 };
    id event = ((id(*)(id, SEL, unsigned long, tray_point, unsigned long, double, long, id, short, long, long))objc_msgSend)(
        (id)objc_getClass("NSEvent"),
        sel_registerName("otherEventWithType:location:modifierFlags:timestamp:windowNumber:context:subtype:data1:data2:"),
        TRAY_NS_EVENT_TYPE_APPLICATION_DEFINED, location, 0UL, 0.0, 0L, (id)NULL, (short)0, 0L, 0L);
    if (event) {
        ((void(*)(id, SEL, id, bool))objc_msgSend)(app, sel_registerName("postEvent:atStart:"), event, false);
    }
    ((void(*)(id, SEL))objc_msgSend)(pool, sel_registerName("drain"));
}

void tray_wake(void) {
    dispatch_async_f(dispatch_get_main_queue(), NULL, _tray_wake_main);
}

void tray_exit() {
    ((void(*)(id, SEL, id))objc_msgSend)(app, sel_registerName("terminate:"), app);
    rootMenu = NULL;
}

#elif defined(TRAY_WINAPI)

#define UNICODE
#define _UNICODE

#include <windows.h>
#include <shellapi.h>
#include <Shobjidl.h>

#define WM_TRAY_CALLBACK_MESSAGE (WM_USER + 1)
#define WC_TRAY_CLASS_NAME L"com.zerotier.zerotier.DesktopUI"
#define ID_TRAY_FIRST 1000

static WNDCLASSEX wc;
static NOTIFYICONDATA nid;
static HWND hwnd;
static HMENU hmenu = NULL;
static UINT_PTR window_timer = 0;

static LRESULT CALLBACK _tray_wnd_proc(HWND hwnd, UINT msg, WPARAM wparam,
                                       LPARAM lparam) {
  switch (msg) {
  case WM_CLOSE:
    DestroyWindow(hwnd);
    return 0;
  case WM_DESTROY:
    PostQuitMessage(0);
    return 0;
  case WM_TRAY_CALLBACK_MESSAGE:
    if (lparam == WM_LBUTTONUP || lparam == WM_RBUTTONUP) {
      POINT p;
      GetCursorPos(&p);
      SetForegroundWindow(hwnd);
      WORD cmd = TrackPopupMenu(hmenu, TPM_LEFTALIGN | TPM_RIGHTBUTTON |
                                           TPM_RETURNCMD | TPM_NONOTIFY,
                                p.x, p.y, 0, hwnd, NULL);
      SendMessage(hwnd, WM_COMMAND, cmd, 0);
      return 0;
    }
    break;
  case WM_COMMAND:
    if (wparam >= ID_TRAY_FIRST) {
      MENUITEMINFO item = {
          .cbSize = sizeof(MENUITEMINFO), .fMask = MIIM_ID | MIIM_DATA,
      };
      if (GetMenuItemInfo(hmenu, wparam, FALSE, &item)) {
        struct tray_menu *menu = (struct tray_menu *)item.dwItemData;
        if (menu != NULL && menu->cb != NULL) {
          menu->cb(menu);
        }
      }
      return 0;
    }
    break;
  }
  return DefWindowProc(hwnd, msg, wparam, lparam);
}

static HMENU _tray_menu(struct tray_menu *m, UINT *id) {
  HMENU hmenu = CreatePopupMenu();
  for (; m != NULL && m->wtext != NULL; m++, (*id)++) {
    if (!wcscmp(m->wtext, L"-")) {
      InsertMenuW(hmenu, *id, MF_SEPARATOR, TRUE, L"");
    } else {
      MENUITEMINFOW item;
      memset(&item, 0, sizeof(item));
      item.cbSize = sizeof(MENUITEMINFOW);
      item.fMask = MIIM_ID | MIIM_TYPE | MIIM_STATE | MIIM_DATA;
      item.fType = 0;
      item.fState = 0;
      if (m->submenu != NULL) {
        item.fMask = item.fMask | MIIM_SUBMENU;
        item.hSubMenu = _tray_menu(m->submenu, id);
      }
      if (m->disabled) {
        item.fState |= MFS_DISABLED;
      }
      if (m->checked) {
        item.fState |= MFS_CHECKED;
      }
      item.wID = *id;
      item.dwTypeData = m->wtext;
      item.dwItemData = (ULONG_PTR)m;
      InsertMenuItemW(hmenu, *id, TRUE, &item);
    }
  }
  return hmenu;
}

SHSTDAPI SetCurrentProcessExplicitAppUserModelID(PCWSTR AppID);

int tray_init(struct tray *tray) {
  SetCurrentProcessExplicitAppUserModelID(L"com.zerotier.zerotier.DesktopUI");
  memset(&wc, 0, sizeof(wc));
  wc.cbSize = sizeof(WNDCLASSEX);
  wc.lpfnWndProc = _tray_wnd_proc;
  wc.hInstance = GetModuleHandle(NULL);
  wc.lpszClassName = WC_TRAY_CLASS_NAME;
  if (!RegisterClassEx(&wc)) {
    return -1;
  }

  hwnd = CreateWindowExW(0, WC_TRAY_CLASS_NAME, NULL, 0, 0, 0, 0, 0, 0, 0, 0, 0);
  if (hwnd == NULL) {
    return -1;
  }
  UpdateWindow(hwnd);

  memset(&nid, 0, sizeof(nid));
  nid.cbSize = sizeof(NOTIFYICONDATA);
  nid.hWnd = hwnd;
  nid.uID = 0;
  nid.uFlags = NIF_ICON | NIF_MESSAGE;
  nid.uCallbackMessage = WM_TRAY_CALLBACK_MESSAGE;
  Shell_NotifyIcon(NIM_ADD, &nid);

  tray_update(tray);

  window_timer = SetTimer(NULL, 0, (MAX_WAIT_SECONDS * 1000), NULL);

  return 0;
}

int tray_loop(int blocking) {
  MSG msg;
  if (blocking) {
    GetMessage(&msg, NULL, 0, 0);
  } else {
    PeekMessage(&msg, NULL, 0, 0, PM_REMOVE);
  }
  if (msg.message == WM_QUIT) {
    return -1;
  }
  TranslateMessage(&msg);
  DispatchMessage(&msg);
  return 0;
}

void tray_update(struct tray *tray) {
  HMENU prevmenu = hmenu;
  UINT id = ID_TRAY_FIRST;
  hmenu = _tray_menu(tray->menu, &id);
  SendMessage(hwnd, WM_INITMENUPOPUP, (WPARAM)hmenu, 0);
  HICON icon;
  ExtractIconExA(tray->icon, 0, NULL, &icon, 1);
  if (nid.hIcon) {
    DestroyIcon(nid.hIcon);
  }
  nid.hIcon = icon;
  Shell_NotifyIcon(NIM_MODIFY, &nid);

  if (prevmenu != NULL) {
    DestroyMenu(prevmenu);
  }
}

void tray_exit() {
  if (window_timer != 0)
    KillTimer(NULL, window_timer);
  window_timer = 0;
  Shell_NotifyIcon(NIM_DELETE, &nid);
  if (nid.hIcon != 0) {
    DestroyIcon(nid.hIcon);
  }
  if (hmenu != 0) {
    DestroyMenu(hmenu);
  }
  PostQuitMessage(0);
  UnregisterClass(WC_TRAY_CLASS_NAME, GetModuleHandle(NULL));
}

/* PostMessage() may be called from any thread; GetMessage() in tray_loop()
 * returns once the message arrives, and WM_NULL is ignored by the window. */
void tray_wake(void) {
  if (hwnd != NULL) {
    PostMessage(hwnd, WM_NULL, 0, 0);
  }
}
#else

int tray_init(struct tray *tray)
{ return -1; }

int tray_loop(int blocking)
{ return -1; }

void tray_update(struct tray *tray)
{}

void tray_exit();

void tray_wake(void)
{}

#endif

#endif /* TRAY_H */
