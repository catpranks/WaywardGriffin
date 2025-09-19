//usr/bin/env gcc -Wall -Werror -std=c17 -O2 "$0" -o "${0%.c}" -lm $(pkg-config --cflags --libs x11 xpresent) && exec "${0%.c}" "$@"

#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/extensions/Xpresent.h>
#include <X11/extensions/presenttokens.h>
#include <time.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <math.h>

static double ms_since(struct timespec a, struct timespec b) {
    return (b.tv_sec - a.tv_sec) * 1000.0 + (b.tv_nsec - a.tv_nsec) / 1e6;
}

static void calculate_stats(const char *name, const double *data, int n, int show_fps) {
    if (n <= 0) {
        printf("No data for %s to calculate stats.\n", name);
        return;
    }
    double sum = 0;
    double min = data[0];
    double max = data[0];
    for (int i = 0; i < n; i++) {
        sum += data[i];
        if (data[i] < min) min = data[i];
        if (data[i] > max) max = data[i];
    }
    double avg = sum / n;

    double sd_sum = 0;
    for (int i = 0; i < n; i++) {
        sd_sum += (data[i] - avg) * (data[i] - avg);
    }
    double stddev = sqrt(sd_sum / n);

    printf("\n--- Stats for %s (%d samples) ---\n", name, n);
    printf("Avg:    %.3f ms\n", avg);
    printf("Stddev: %.3f ms\n", stddev);
    printf("Min:    %.3f ms\n", min);
    printf("Max:    %.3f ms\n", max);

    if (show_fps) {
        printf("Avg FPS: %.2f\n", 1000.0 / avg);
        printf("Min FPS: %.2f\n", 1000.0 / max);
        printf("Max FPS: %.2f\n", 1000.0 / min);
    }
}

int main(void) {
    Display *dpy = XOpenDisplay(NULL);
    if (!dpy) { fprintf(stderr, "no DISPLAY\n"); return 1; }

    int opcode, evbase, errbase;
    if (!XPresentQueryExtension(dpy, &opcode, &evbase, &errbase)) {
        fprintf(stderr, "Present not supported\n"); return 1;
    }
    int srv_major, srv_minor;
    XPresentQueryVersion(dpy, &srv_major, &srv_minor);

    int scr = DefaultScreen(dpy);
    Window root = RootWindow(dpy, scr);
    unsigned w = 640, h = 480;

    Window win = XCreateSimpleWindow(dpy, root, 100, 100, w, h, 0,
                                     BlackPixel(dpy, scr), BlackPixel(dpy, scr));
    XSelectInput(dpy, win, StructureNotifyMask);     // for MapNotify
    XMapWindow(dpy, win);
    XFlush(dpy);

    // Wait until mapped
    for (;;) {
        XEvent e; XNextEvent(dpy, &e);
        if (e.type == MapNotify && e.xmap.window == win) break;
    }

    // Ask for Present completion events
    XID eid = XPresentSelectInput(dpy, win, PresentCompleteNotifyMask);

    // Make a simple pixmap to present
    unsigned depth = DefaultDepth(dpy, scr);
    Pixmap pm = XCreatePixmap(dpy, win, w, h, depth);
    GC gc = XCreateGC(dpy, pm, 0, NULL);
    XSetForeground(dpy, gc, WhitePixel(dpy, scr));
    XFillRectangle(dpy, pm, gc, 0, 0, w, h);

    // Present and time until completion
    const int num_warmup = 10;
    const int num_presents = 100;
    const int total_presents = num_presents + num_warmup;
    double *latencies = malloc(total_presents * sizeof(double));
    double *frame_times = malloc((total_presents > 1 ? total_presents - 1 : 1) * sizeof(double));
    if (!latencies || !frame_times) { fprintf(stderr, "malloc failed\n"); return 1; }

    struct timespec last_completion_time = {0};
    uint32_t serial = 1;
    for (int i = 0; i < total_presents; i++) {
        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);

        XPresentPixmap(dpy, win, pm, serial,
                       None, None, 0, 0,
                       None, None, None,
                       0, /* options */
                       0, 0, 0,
                       NULL, 0);
        XFlush(dpy);

        for (;;) {
            XEvent e; XNextEvent(dpy, &e);
            if (e.type != GenericEvent) continue;
            XGenericEventCookie *c = &e.xcookie;
            if (c->extension != opcode) continue;
            if (c->evtype != PresentCompleteNotify) continue;
            if (!XGetEventData(dpy, c)) continue;

            XPresentCompleteNotifyEvent *ce = (XPresentCompleteNotifyEvent *)c->data;
            if (ce->serial_number == serial) {
                clock_gettime(CLOCK_MONOTONIC, &t1);
                double ms = ms_since(t0, t1);
                latencies[i] = ms;

                if (i > 0) {
                    frame_times[i - 1] = ms_since(last_completion_time, t1);
                }
                last_completion_time = t1;

                if (i < num_warmup) {
                     printf("Warmup %d/%d: latency=%.3f ms\n", i + 1, num_warmup, ms);
                } else {
                     printf("Present complete: serial=%u kind=%u mode=%u msc=%llu ust=%llu latency=%.3f ms\n",
                           (unsigned int)serial, ce->kind, ce->mode,
                           (unsigned long long)ce->msc,
                           (unsigned long long)ce->ust,
                           ms);
                }
                XFreeEventData(dpy, c);
                break;
            }
            XFreeEventData(dpy, c);
        }
        serial++;
    }

    calculate_stats("Present Latency", latencies + num_warmup, num_presents, 0);
    if (total_presents > 1 + num_warmup) {
        calculate_stats("Frame Times", frame_times + num_warmup -1, num_presents, 1);
    }


    free(latencies);
    free(frame_times);
    XPresentFreeInput(dpy, win, eid);
    XFreeGC(dpy, gc);
    XFreePixmap(dpy, pm);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return 0;
}
