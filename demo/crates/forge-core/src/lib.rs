mod temperature;

pub use temperature::forge_temperature;

pub fn status() -> String {
    format!("the forge is burning at {}°", forge_temperature())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_the_temperature() {
        assert!(super::status().contains("42°"));
    }
}
