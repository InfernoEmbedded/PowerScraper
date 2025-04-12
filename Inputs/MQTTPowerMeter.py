#!/usr/bin/env python3
"""
MQTTPowerMeter.py

This module implements an input that subscribes to an MQTT broker.
It reads the payloads from configurable topics and exposes the values as:
  - 'Total system power'
  - 'Phase 1 power'
  - 'Phase 2 power'
  - 'Phase 3 power'
for use by the rest of the system.
Configuration is taken from the config file in the section keyed by the meter name.
"""

import paho.mqtt.client as mqtt

class MQTTPowerMeter(object):
    def __init__(self, config, name):
        """
        Initializes the MQTT Power Meter.

        Expected configuration keys under config[name]:
          - broker: MQTT server address.
          - port: MQTT server port (default: 1883).
          - topic_total: (Optional) Topic for Total system power (default: "TotalSystemPower").
          - topic_phase1: (Optional) Topic for Phase 1 power (default: "Phase1Power").
          - topic_phase2: (Optional) Topic for Phase 2 power (default: "Phase2Power").
          - topic_phase3: (Optional) Topic for Phase 3 power (default: "Phase3Power").
          - username: (Optional) MQTT username.
          - password: (Optional) MQTT password.
        """
        self.name = name
        self.config = config

        # Latest readings for each metric.
        self.latest_total = None
        self.latest_phase1 = None
        self.latest_phase2 = None
        self.latest_phase3 = None

        # Read topics from the configuration; use defaults if not provided.
        self.topic_total = self.config[self.name].get("topic_total", "TotalSystemPower")
        self.topic_phase1 = self.config[self.name].get("topic_phase1", "Phase1Power")
        self.topic_phase2 = self.config[self.name].get("topic_phase2", "Phase2Power")
        self.topic_phase3 = self.config[self.name].get("topic_phase3", "Phase3Power")

        # Create the MQTT client instance.
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)

        # Set username and password if provided.
        if "username" in config[self.name] and config[self.name]["username"]:
            self.client.username_pw_set(config[self.name]["username"], config[self.name].get("password", ""))

        # Set callback functions.
        self.client.on_connect = self.on_connect
        self.client.on_message = self.on_message

        # Connect to the MQTT broker.
        broker = config[self.name]["broker"]
        port = int(config[self.name].get("port", 1883))
        self.client.connect(broker, port)

        # Start the network loop in a separate thread.
        self.client.loop_start()

    def on_connect(self, client, userdata, flags, reason_code, properties):
        """Called upon connection to the MQTT broker."""
        print(f"MQTTPowerMeter ({self.name}): Connected to broker with result code {reason_code}")
        # Subscribe to each topic.
        print(f"MQTTPowerMeter ({self.name}): Subscribing to topic: {self.topic_total}")
        client.subscribe(self.topic_total)
        print(f"MQTTPowerMeter ({self.name}): Subscribing to topic: {self.topic_phase1}")
        client.subscribe(self.topic_phase1)
        print(f"MQTTPowerMeter ({self.name}): Subscribing to topic: {self.topic_phase2}")
        client.subscribe(self.topic_phase2)
        print(f"MQTTPowerMeter ({self.name}): Subscribing to topic: {self.topic_phase3}")
        client.subscribe(self.topic_phase3)

    def on_message(self, client, userdata, msg):
        """Called when a message is received from the broker."""
        try:
            value = float(msg.payload.decode("utf-8"))
        except Exception as e:
            print(f"MQTTPowerMeter ({self.name}): Error decoding message on topic {msg.topic} -", e)
            return

        if msg.topic == self.topic_total:
            self.latest_total = value
            print(f"MQTTPowerMeter ({self.name}): Received Total system power value: {self.latest_total}")
        elif msg.topic == self.topic_phase1:
            self.latest_phase1 = value
            print(f"MQTTPowerMeter ({self.name}): Received Phase 1 power value: {self.latest_phase1}")
        elif msg.topic == self.topic_phase2:
            self.latest_phase2 = value
            print(f"MQTTPowerMeter ({self.name}): Received Phase 2 power value: {self.latest_phase2}")
        elif msg.topic == self.topic_phase3:
            self.latest_phase3 = value
            print(f"MQTTPowerMeter ({self.name}): Received Phase 3 power value: {self.latest_phase3}")
        else:
            print(f"MQTTPowerMeter ({self.name}): Unhandled topic: {msg.topic}")

    def fetch(self, completionCallback):
        """
        Called periodically by the input loop. The provided completionCallback
        is called with a dictionary that includes the keys:
            - 'Total system power'
            - 'Phase 1 power'
            - 'Phase 2 power'
            - 'Phase 3 power'
        and their respective latest values (or 0.0 if no value has been received yet).
        """
        reading = {
            "name": self.name,
            "Total system power": self.latest_total if self.latest_total is not None else 0.0,
            "Phase 1 power": self.latest_phase1 if self.latest_phase1 is not None else 0.0,
            "Phase 2 power": self.latest_phase2 if self.latest_phase2 is not None else 0.0,
            "Phase 3 power": self.latest_phase3 if self.latest_phase3 is not None else 0.0,
        }
        completionCallback(reading, None)

