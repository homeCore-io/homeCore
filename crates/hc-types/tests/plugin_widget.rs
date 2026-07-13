#[test]
fn plugin_widget_wire_value_and_copy_preserved() {
    use hc_types::dashboard::DashboardWidgetType as T;
    // The Dart side hard-codes this string as `pluginWidgetType`.
    assert_eq!(
        serde_json::to_string(&T::PluginWidget).unwrap(),
        "\"plugin_widget\""
    );
    let round: T = serde_json::from_str("\"plugin_widget\"").unwrap();
    assert_eq!(round, T::PluginWidget);
    // Still Copy — this is why we did not add Custom(String).
    let a = T::PluginWidget;
    let b = a;
    assert_eq!(a, b);
}
