# Home monitoring system made to learn async Rust for embedded

Details:

- Purpose of the project is to monitor current temperature, humidity and movement in a certain room, and whether a certain door gets opened
- Data is read from sensors in order to send it over MQTT for further processing
- UI is displayed on an SSD1309 display, it allows the user to view current sensor values, modify delays between reads, or disable each of the sensors and MQTT
- Each of the sensors (aside of DHT11) and buttons have their own threads, which send messages over channels into the main thread
- MQTT related tasks, such as sending messages, keeping the connection alive and reconnecting are also handled in a seperate thread
- Main thread processes all incoming messages, displays the UI and sends data into the MQTT thread over a channel
- Server reads data from MQTT and stores values read from **esp32/temperature esp32/humidity esp32/contact esp32/motion** to an SQLite database
- When message arrives from **esp32/contact esp32/motion**, alerts are sent over Gmail, with credentials provided in .env
