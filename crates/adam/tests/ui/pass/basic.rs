use adam::prelude::*;

struct Units(&'static str);

/// Get the weather for a city.
/// The answer is in the configured units.
#[tool]
pub async fn get_weather(
    units: State<Units>,
    ctx: &ToolCtx,
    /// The city
    city: String,
    days: Option<u32>,
) -> Result<String, ToolError> {
    Ok(format!("{city} {} {:?} {}", units.0, days, ctx.tool_name()))
}

fn main() {
    let spec = GetWeather.spec();
    assert_eq!(spec.name, "get_weather");
    assert_eq!(spec.description, "Get the weather for a city. The answer is in the configured units.");
    assert_eq!(spec.parameters["properties"]["city"]["description"], "The city");
    assert_eq!(GetWeather.required_state().len(), 1);
    let _ = tools![GetWeather];
}
