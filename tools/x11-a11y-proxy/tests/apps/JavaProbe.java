import javax.swing.*;
import java.awt.*;
import java.awt.event.*;
/** Swing probe: label, button "Seven" (mnemonic S, tooltip), check box "Check me" (mnemonic C), text field,
 *  slider 0-100 at 40, Ctrl+K; prints ACTIVATE / TOGGLE / TEXT / VALUE / SHORTCUT and the desktop properties. */
public class JavaProbe {
    public static void main(String[] a) throws Exception {
        String tag = a.length > 0 ? a[0] : "x";
        SwingUtilities.invokeAndWait(() -> {
            JFrame f = new JFrame("Java probe " + tag);
            JPanel p = new JPanel(new GridLayout(5, 1));
            p.add(new JLabel("Probe label"));
            JButton b = new JButton("Seven"); b.setMnemonic('S'); b.setToolTipText("Adds the digit seven");
            b.addActionListener(e -> System.out.println("ACTIVATE Seven"));
            p.add(b);
            JCheckBox c = new JCheckBox("Check me"); c.setMnemonic('C');
            c.addActionListener(e -> System.out.println("TOGGLE " + c.isSelected()));
            p.add(c);
            JTextField t = new JTextField(12);
            t.getDocument().addDocumentListener(new javax.swing.event.DocumentListener() {
                public void insertUpdate(javax.swing.event.DocumentEvent e) { System.out.println("TEXT " + t.getText()); }
                public void removeUpdate(javax.swing.event.DocumentEvent e) { System.out.println("TEXT " + t.getText()); }
                public void changedUpdate(javax.swing.event.DocumentEvent e) {}
            });
            p.add(t);
            JSlider s = new JSlider(0, 100, 40); s.addChangeListener(e -> System.out.println("VALUE " + s.getValue()));
            p.add(s);
            p.getInputMap(JComponent.WHEN_IN_FOCUSED_WINDOW).put(KeyStroke.getKeyStroke("control K"), "ck");
            p.getActionMap().put("ck", new AbstractAction() { public void actionPerformed(ActionEvent e) { System.out.println("SHORTCUT ctrl+k"); } });
            f.add(p); f.pack(); f.setDefaultCloseOperation(JFrame.EXIT_ON_CLOSE); f.setVisible(true);
            Toolkit tk = Toolkit.getDefaultToolkit();
            System.out.println("SETTINGS java {dpi=" + tk.getScreenResolution() + ", gnome.Xft/DPI=" + tk.getDesktopProperty("gnome.Xft/DPI")
                + ", gnome.Net/ThemeName=" + tk.getDesktopProperty("gnome.Net/ThemeName") + ", gnome.Gtk/FontName=" + tk.getDesktopProperty("gnome.Gtk/FontName")
                + ", gnome.Gtk/CursorThemeSize=" + tk.getDesktopProperty("gnome.Gtk/CursorThemeSize")
                + ", uiScale=" + GraphicsEnvironment.getLocalGraphicsEnvironment().getDefaultScreenDevice().getDefaultConfiguration().getDefaultTransform().getScaleX()
                + ", font=" + UIManager.getFont("Label.font") + ", lnf=" + UIManager.getLookAndFeel().getName()
                + ", at=" + System.getProperty("javax.accessibility.assistive_technologies") + "}");
            System.out.println("READY");
        });
    }
}
