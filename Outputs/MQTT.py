"""
MQTT Output Module

This module implements an output class that publishes measurement data
to an MQTT broker using the paho-mqtt library.

Each value is sent under a topic constructed as:
    <base_topic>/<Input Name>/<value key>

The configuration settings expected (as seen in config-sample.toml):
    broker      - MQTT broker address.
    port        - MQTT broker port.
    base_topic  - Base topic for publishing measurement data (e.g. "sensors").
    username    - (Optional) MQTT username.
    password    - (Optional) MQTT password.
"""

import paho.mqtt.client as mqtt


class MQTT(object):
    def __init__(self, config):
        """
        Initializes the MQTT Output instance.

        Expected configuration keys under config:
            broker      - MQTT broker address.
            port        - MQTT broker port (default: 1883).
            base_topic  - Base topic for publishing measurements (default: "sensors").
            username    - (Optional) Username for MQTT authentication.
            password    - (Optional) Password for MQTT authentication.
        """
        self.config = config
        self.broker = config.get("broker", "mqtt.example.com")
        self.port = int(config.get("port", 1883))
        self.base_topic = config.get("base_topic", "sensors")
        self.username = config.get("username")
        self.password = config.get("password")

        # Create the MQTT client instance.
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)

        # Set username and password if provided.
        if self.username:
            self.client.username_pw_set(self.username, self.password)

        # Connect to the MQTT broker.
        self.client.connect(self.broker, self.port)

        # Start the network loop in a separate thread to handle reconnections and callbacks.
        self.client.loop_start()

    def send(self, vals, batteryAPI):
        """
        Publishes the given data dictionary to the MQTT broker.

        Each value is published under a topic constructed as:
            <base_topic>/<Input Name>/<value key>

        The `vals` dictionary should contain a key 'name' that will be used as the
        input name in the topic; all other keys are published as individual messages.
        
        :param vals: Dictionary with data values to publish (e.g., measurements).
        :param batteryAPI: (Unused) Parameter reserved for battery control API.
        """
        # Retrieve input name from the 'name' field (or use a default if missing)
        input_name = vals.get("name", "unknown")
        for key, value in vals.items():
            # Skip the 'name' key since it’s used as the topic’s subfolder.
            if key == "name":
                continue
            # Build the topic according to the format <base_topic>/<Input Name>/<value key>
            topic = f"{self.base_topic}/{input_name}/{key}"
            # Convert the value to string for publishing.
            payload = str(value)
            # Publish the message.
            self.client.publish(topic, payload)

