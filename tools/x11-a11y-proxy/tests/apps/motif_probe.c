/* Motif probe: label, push buttons "Seven" (mnemonic S) and "Plus", toggle "Check me" (mnemonic C), text field,
   scale 0-100 at 40. Prints MOTIF_ACTIVATE / TOGGLE / TEXT / VALUE and the render table it got. */
#include <Xm/Xm.h>
#include <Xm/RowColumn.h>
#include <Xm/Label.h>
#include <Xm/PushB.h>
#include <Xm/ToggleB.h>
#include <Xm/TextF.h>
#include <Xm/Scale.h>
#include <stdio.h>
static void act(Widget w, XtPointer client, XtPointer call) { printf("ACTIVATE %s\n", (char *)client); fflush(stdout); }
static void tog(Widget w, XtPointer client, XtPointer call) { printf("TOGGLE %s\n", XmToggleButtonGetState(w) ? "True" : "False"); fflush(stdout); }
static void txt(Widget w, XtPointer client, XtPointer call) { char *s = XmTextFieldGetString(w); printf("TEXT %s\n", s); XtFree(s); fflush(stdout); }
static void val(Widget w, XtPointer client, XtPointer call) { int v; XmScaleGetValue(w, &v); printf("VALUE %d\n", v); fflush(stdout); }
int main(int argc, char **argv) {
    XtAppContext app;
    Widget top = XtVaOpenApplication(&app, "MotifProbe", NULL, 0, &argc, argv, NULL, sessionShellWidgetClass,
                                     XmNtitle, argc > 1 ? argv[1] : "Motif probe", NULL);
    Widget rc = XmCreateRowColumn(top, "rc", NULL, 0);
    XtManageChild(XmCreateLabel(rc, "Probe label", NULL, 0));
    Widget b1 = XmCreatePushButton(rc, "Seven", NULL, 0);
    XtVaSetValues(b1, XmNmnemonic, XStringToKeysym("S"), NULL);
    XtAddCallback(b1, XmNactivateCallback, act, "Seven"); XtManageChild(b1);
    Widget b2 = XmCreatePushButton(rc, "Plus", NULL, 0);
    XtAddCallback(b2, XmNactivateCallback, act, "Plus"); XtManageChild(b2);
    Widget t = XmCreateToggleButton(rc, "Check me", NULL, 0);
    XtVaSetValues(t, XmNmnemonic, XStringToKeysym("C"), NULL);
    XtAddCallback(t, XmNvalueChangedCallback, tog, NULL); XtManageChild(t);
    Widget e = XmCreateTextField(rc, "entry", NULL, 0);
    XtAddCallback(e, XmNvalueChangedCallback, txt, NULL); XtManageChild(e);
    Widget s = XmCreateScale(rc, "level", NULL, 0);
    XtVaSetValues(s, XmNorientation, XmHORIZONTAL, XmNminimum, 0, XmNmaximum, 100, XmNvalue, 40, NULL);
    XtAddCallback(s, XmNvalueChangedCallback, val, NULL); XtManageChild(s);
    XtManageChild(rc);
    XtRealizeWidget(top);
    printf("READY\n"); fflush(stdout);
    XtAppMainLoop(app);
    return 0;
}
